//! Elevación de prioridad de los hilos de tiempo real (spec §2.3.2).
//!
//! * Linux: `SCHED_FIFO` con prioridad RT (90 para audio, 80 para el secuenciador) y `mlockall` para
//!   evitar fallos de página. Requiere `RLIMIT_RTPRIO` (grupo `realtime` en CachyOS).
//! * Windows: clase de proceso `REALTIME_PRIORITY_CLASS` (con respaldo `HIGH_PRIORITY_CLASS`) y MMCSS
//!   `"Pro Audio"` con prioridad crítica por hilo (respaldo `THREAD_PRIORITY_TIME_CRITICAL`).
//!
//! Nada de esto es fatal: sin privilegios el daemon sigue funcionando con aviso.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadRole {
    Audio,
    Sequencer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityError {
    pub message: String,
}

impl std::fmt::Display for PriorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PriorityError {}

/// Qué se logró, para el registro de arranque.
pub type PriorityOutcome = Result<String, PriorityError>;

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    pub fn elevate_current_thread(role: ThreadRole, priority: u8) -> PriorityOutcome {
        if role == ThreadRole::Sequencer {
            // El temporizador no debe redondear las esperas hacia arriba (50 µs por defecto).
            // SAFETY: `prctl` con una opción numérica y un valor entero.
            unsafe {
                libc::prctl(libc::PR_SET_TIMERSLACK, 1u64, 0u64, 0u64, 0u64);
            }
        }
        let prio = priority.clamp(1, 99) as i32;
        let param = libc::sched_param { sched_priority: prio };
        // SAFETY: `param` es válido durante la llamada y apunta al hilo actual.
        let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
        if rc == 0 {
            Ok(format!("SCHED_FIFO prioridad {prio}"))
        } else {
            Err(PriorityError {
                message: format!(
                    "no se pudo fijar SCHED_FIFO {prio}: {} (¿el usuario está en el grupo `realtime`?)",
                    std::io::Error::from_raw_os_error(rc)
                ),
            })
        }
    }

    pub fn lock_process_memory() -> PriorityOutcome {
        // SAFETY: sin punteros; solo banderas.
        let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
        if rc == 0 {
            Ok("mlockall(MCL_CURRENT | MCL_FUTURE)".into())
        } else {
            Err(PriorityError { message: format!("mlockall falló: {} (límite memlock)", std::io::Error::last_os_error()) })
        }
    }

    pub fn raise_process_priority() -> PriorityOutcome {
        Ok("sin clase de proceso en Linux".into())
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentProcess, GetCurrentThread, SetPriorityClass,
        SetThreadPriority, AVRT_PRIORITY_CRITICAL, HIGH_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS, THREAD_PRIORITY_TIME_CRITICAL,
    };

    pub fn elevate_current_thread(_role: ThreadRole, _priority: u8) -> PriorityOutcome {
        let mut task_index = 0u32;
        // SAFETY: cadena UTF-16 terminada en NUL y puntero a una variable local.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(handle) => {
                // SAFETY: `handle` acaba de devolverlo MMCSS.
                let _ = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL) };
                Ok("MMCSS \"Pro Audio\", prioridad crítica".into())
            }
            Err(e) => {
                // SAFETY: operan sobre el hilo actual.
                let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }.is_ok();
                if ok {
                    Ok(format!("THREAD_PRIORITY_TIME_CRITICAL (MMCSS no disponible: {e})"))
                } else {
                    Err(PriorityError { message: format!("MMCSS y SetThreadPriority fallaron: {e}") })
                }
            }
        }
    }

    pub fn lock_process_memory() -> PriorityOutcome {
        Ok("sin bloqueo de memoria en Windows".into())
    }

    pub fn raise_process_priority() -> PriorityOutcome {
        // SAFETY: operan sobre el proceso actual.
        unsafe {
            let process = GetCurrentProcess();
            if SetPriorityClass(process, REALTIME_PRIORITY_CLASS).is_ok() {
                Ok("REALTIME_PRIORITY_CLASS".into())
            } else if SetPriorityClass(process, HIGH_PRIORITY_CLASS).is_ok() {
                Ok("HIGH_PRIORITY_CLASS (sin SeIncreaseBasePriorityPrivilege)".into())
            } else {
                Err(PriorityError { message: "SetPriorityClass falló".into() })
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    use super::*;

    pub fn elevate_current_thread(_role: ThreadRole, _priority: u8) -> PriorityOutcome {
        Err(PriorityError { message: "elevación de prioridad no soportada en esta plataforma".into() })
    }

    pub fn lock_process_memory() -> PriorityOutcome {
        Err(PriorityError { message: "mlockall no soportado en esta plataforma".into() })
    }

    pub fn raise_process_priority() -> PriorityOutcome {
        Ok("sin clase de proceso".into())
    }
}

/// Eleva el hilo actual. `priority` solo cuenta en Linux (1..=99).
pub fn elevate_current_thread(role: ThreadRole, priority: u8) -> PriorityOutcome {
    imp::elevate_current_thread(role, priority)
}

/// Fija las páginas del proceso en RAM (Linux). Llamar una vez al arrancar.
pub fn lock_process_memory() -> PriorityOutcome {
    imp::lock_process_memory()
}

/// Sube la clase de prioridad del proceso (Windows). Llamar una vez al arrancar.
pub fn raise_process_priority() -> PriorityOutcome {
    imp::raise_process_priority()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_never_panics_and_reports_why_it_failed() {
        // Según los privilegios del entorno de pruebas puede lograrse o no; ambos son válidos.
        std::thread::spawn(|| {
            for role in [ThreadRole::Audio, ThreadRole::Sequencer] {
                match elevate_current_thread(role, 90) {
                    Ok(msg) => assert!(!msg.is_empty()),
                    Err(e) => assert!(!e.to_string().is_empty()),
                }
            }
        })
        .join()
        .unwrap();
        let _ = raise_process_priority();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn out_of_range_priorities_are_clamped_not_rejected_as_invalid() {
        std::thread::spawn(|| {
            // 0 y 255 se recortan a 1 y 99; el único fallo posible es de permisos (EPERM), no EINVAL.
            for p in [0u8, 255] {
                if let Err(e) = elevate_current_thread(ThreadRole::Audio, p) {
                    assert!(!e.message.contains("Invalid argument"), "{e}");
                }
            }
        })
        .join()
        .unwrap();
    }
}

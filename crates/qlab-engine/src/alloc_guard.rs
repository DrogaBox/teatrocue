//! `GlobalAlloc` que detecta asignaciones/liberaciones dentro del callback de audio (spec §2.2.1).
//!
//! El binario registra el envoltorio:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: qlab_engine::alloc_guard::RtGuardAllocator = qlab_engine::alloc_guard::RtGuardAllocator::system();
//! ```
//!
//! y el callback se ejecuta dentro de un [`RtScope`] (o [`assert_no_alloc`]). Cualquier `alloc`,
//! `dealloc`, `alloc_zeroed` o `realloc` en ese hilo y ese ámbito es una violación.
//!
//! La política por defecto es abortar con backtrace, no hacer `panic!`: un panic que sale de un
//! `GlobalAlloc` es comportamiento indefinido según la documentación de `std`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, Ordering};

// Inicialización `const` y sin destructor: acceder desde el asignador no asigna ni se registra.
thread_local! {
    static IN_RT_CALLBACK: Cell<bool> = const { Cell::new(false) };
    static RT_VIOLATIONS: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ViolationPolicy {
    /// Imprime el backtrace y aborta el proceso.
    Abort = 0,
    /// Cuenta la violación en el hilo y deja pasar la operación (tests).
    CountOnly = 1,
}

static POLICY: AtomicU8 = AtomicU8::new(ViolationPolicy::Abort as u8);

pub fn set_violation_policy(policy: ViolationPolicy) {
    POLICY.store(policy as u8, Ordering::Relaxed);
}

#[inline]
pub fn in_rt_callback() -> bool {
    IN_RT_CALLBACK.try_with(Cell::get).unwrap_or(false)
}

/// Violaciones contadas en este hilo con [`ViolationPolicy::CountOnly`].
pub fn violations_on_this_thread() -> u64 {
    RT_VIOLATIONS.try_with(Cell::get).unwrap_or(0)
}

/// Marca el hilo actual como "dentro del callback RT" hasta que se suelte. Admite anidamiento.
pub struct RtScope {
    previous: bool,
    // `!Send`: debe soltarse en el hilo que lo creó.
    _not_send: PhantomData<*const ()>,
}

impl RtScope {
    #[inline]
    pub fn enter() -> Self {
        let previous = IN_RT_CALLBACK.try_with(|f| f.replace(true)).unwrap_or(false);
        Self { previous, _not_send: PhantomData }
    }
}

impl Drop for RtScope {
    #[inline]
    fn drop(&mut self) {
        let _ = IN_RT_CALLBACK.try_with(|f| f.set(self.previous));
    }
}

/// Ejecuta `f` vigilando asignaciones.
#[inline]
pub fn assert_no_alloc<R>(f: impl FnOnce() -> R) -> R {
    let _scope = RtScope::enter();
    f()
}

/// Permite asignar dentro de `f` aunque haya un [`RtScope`] activo (solo para inicialización).
pub fn permit_alloc<R>(f: impl FnOnce() -> R) -> R {
    let previous = IN_RT_CALLBACK.try_with(|c| c.replace(false)).unwrap_or(false);
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = IN_RT_CALLBACK.try_with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(previous);
    f()
}

#[cold]
#[inline(never)]
fn on_violation(op: &'static str, size: usize) {
    if POLICY.load(Ordering::Relaxed) == ViolationPolicy::CountOnly as u8 {
        let _ = RT_VIOLATIONS.try_with(|c| c.set(c.get() + 1));
        return;
    }
    // Desarmar antes de formatear: imprimir y capturar el backtrace asignan.
    let _ = IN_RT_CALLBACK.try_with(|f| f.set(false));
    eprintln!(
        "[TeatroCue motor] VIOLACIÓN RT: `{op}` de {size} bytes dentro del callback de audio\n{}",
        std::backtrace::Backtrace::force_capture()
    );
    std::process::abort();
}

#[inline(always)]
fn check(op: &'static str, size: usize) {
    if in_rt_callback() {
        on_violation(op, size);
    }
}

pub struct RtGuardAllocator<A = System> {
    inner: A,
}

impl<A> RtGuardAllocator<A> {
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

impl RtGuardAllocator<System> {
    pub const fn system() -> Self {
        Self::new(System)
    }
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for RtGuardAllocator<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check("alloc", layout.size());
        unsafe { self.inner.alloc(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check("dealloc", layout.size());
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        check("alloc_zeroed", layout.size());
        unsafe { self.inner.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check("realloc", new_size);
        unsafe { self.inner.realloc(ptr, layout, new_size) }
    }
}

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: RtGuardAllocator = RtGuardAllocator::system();

#[cfg(test)]
mod tests {
    use super::*;

    fn counting() {
        set_violation_policy(ViolationPolicy::CountOnly);
    }

    #[test]
    fn detects_alloc_and_dealloc_inside_scope() {
        counting();
        let before = violations_on_this_thread();
        assert_no_alloc(|| {
            let v = vec![1u8; 128]; // alloc
            std::hint::black_box(&v);
        }); // dealloc dentro del cierre
        assert!(violations_on_this_thread() >= before + 2);
    }

    #[test]
    fn detects_dealloc_of_outside_allocation() {
        counting();
        let boxed = std::hint::black_box(Box::new([0u8; 64]));
        let before = violations_on_this_thread();
        assert_no_alloc(move || drop(boxed));
        assert!(violations_on_this_thread() > before);
    }

    #[test]
    fn silent_outside_scope_and_for_stack_work() {
        counting();
        let before = violations_on_this_thread();
        let v = vec![0u8; 32];
        drop(v);
        let sum = assert_no_alloc(|| (0..100u32).sum::<u32>());
        assert_eq!(sum, 4950);
        assert_eq!(violations_on_this_thread(), before);
    }

    #[test]
    fn scopes_nest_and_restore() {
        assert!(!in_rt_callback());
        {
            let _a = RtScope::enter();
            {
                let _b = RtScope::enter();
                assert!(in_rt_callback());
            }
            assert!(in_rt_callback()); // sigue activo tras salir del interno
            assert!(!permit_alloc(in_rt_callback));
            assert!(in_rt_callback());
        }
        assert!(!in_rt_callback());
    }

    #[test]
    fn permit_alloc_allows_setup() {
        counting();
        let before = violations_on_this_thread();
        assert_no_alloc(|| {
            permit_alloc(|| drop(vec![0u8; 16]));
        });
        assert_eq!(violations_on_this_thread(), before);
    }

    #[test]
    fn flag_is_per_thread() {
        counting();
        let _scope = RtScope::enter();
        std::thread::spawn(|| assert!(!in_rt_callback())).join().unwrap();
    }
}

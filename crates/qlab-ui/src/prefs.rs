//! Preferencias del operador (`preferencias.json`, junto a los ajustes de audio): cómo se comporta la parada en los ensayos,
//! el silencio de los sonidos de Windows y los **atajos de teclado** a gusto de cada sonidista.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Key, Modifiers};
use serde::{Deserialize, Serialize};

/// Órdenes que se pueden asignar a una tecla.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Command {
    Go,
    Panic,
    Pause,
    Memory,
    Up,
    Down,
    Lock,
    Carts,
    Delete,
    Duplicate,
    Undo,
    Redo,
    New,
    Open,
    Save,
    SaveAs,
    Quit,
    Help,
}

impl Command {
    pub const ALL: [Command; 18] = [
        Command::Go,
        Command::Panic,
        Command::Pause,
        Command::Memory,
        Command::Up,
        Command::Down,
        Command::Lock,
        Command::Carts,
        Command::Delete,
        Command::Duplicate,
        Command::Undo,
        Command::Redo,
        Command::New,
        Command::Open,
        Command::Save,
        Command::SaveAs,
        Command::Quit,
        Command::Help,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Command::Go => "GO (disparar la cue en espera)",
            Command::Panic => "PANIC (dos veces seguidas: parada dura)",
            Command::Pause => "Pausar / reanudar",
            Command::Memory => "Marcar memoria",
            Command::Up => "Selección arriba",
            Command::Down => "Selección abajo",
            Command::Lock => "Bloqueo de función",
            Command::Carts => "Ventana de Carts",
            Command::Delete => "Borrar la cue seleccionada",
            Command::Duplicate => "Duplicar cue",
            Command::Undo => "Deshacer",
            Command::Redo => "Rehacer",
            Command::New => "Show nuevo",
            Command::Open => "Abrir show",
            Command::Save => "Guardar",
            Command::SaveAs => "Guardar como",
            Command::Quit => "Salir",
            Command::Help => "Manual de uso",
        }
    }

    /// Las teclas de fábrica: la primera y, si hay, una segunda.
    pub fn defaults(self) -> [Option<Binding>; 2] {
        let k = |key| Some(Binding::plain(key));
        let c = |key| Some(Binding { key, ctrl: true, shift: false, alt: false });
        let cs = |key| Some(Binding { key, ctrl: true, shift: true, alt: false });
        match self {
            Command::Go => [k(Key::Space), None],
            Command::Panic => [k(Key::Escape), None],
            Command::Pause => [k(Key::P), None],
            Command::Memory => [k(Key::M), k(Key::Enter)],
            Command::Up => [k(Key::ArrowUp), None],
            Command::Down => [k(Key::ArrowDown), None],
            Command::Lock => [c(Key::L), None],
            Command::Carts => [k(Key::F3), None],
            Command::Delete => [k(Key::Delete), k(Key::Backspace)],
            Command::Duplicate => [c(Key::D), None],
            Command::Undo => [c(Key::Z), None],
            Command::Redo => [cs(Key::Z), c(Key::Y)],
            Command::New => [c(Key::N), None],
            Command::Open => [c(Key::O), None],
            Command::Save => [c(Key::S), None],
            Command::SaveAs => [cs(Key::S), None],
            Command::Quit => [c(Key::Q), None],
            Command::Help => [k(Key::F1), None],
        }
    }

    /// Mantener la tecla apretada repite la orden (solo para moverse por la lista; nunca un GO tras otro).
    fn repeats(self) -> bool {
        matches!(self, Command::Up | Command::Down)
    }

    /// GO y PANIC siempre tienen que tener una tecla.
    pub fn required(self) -> bool {
        matches!(self, Command::Go | Command::Panic)
    }
}

/// Una tecla con sus modificadores exactos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub key: Key,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Binding {
    pub const fn plain(key: Key) -> Self {
        Self { key, ctrl: false, shift: false, alt: false }
    }

    pub fn from_event(key: Key, m: Modifiers) -> Self {
        Self { key, ctrl: m.command || m.ctrl, shift: m.shift, alt: m.alt }
    }

    fn matches(&self, key: Key, m: Modifiers) -> bool {
        self.key == key && self.ctrl == (m.command || m.ctrl) && self.shift == m.shift && self.alt == m.alt
    }

    /// Texto estable para el archivo: «Ctrl+Shift+S», «Space», «F3».
    pub fn to_config(self) -> String {
        format!("{}{}{}{}", if self.ctrl { "Ctrl+" } else { "" }, if self.shift { "Shift+" } else { "" }, if self.alt { "Alt+" } else { "" }, self.key.name())
    }

    pub fn from_config(s: &str) -> Option<Self> {
        let mut b = Binding::plain(Key::Space);
        let mut rest = s.trim();
        loop {
            if let Some(r) = rest.strip_prefix("Ctrl+") {
                b.ctrl = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("Shift+") {
                b.shift = true;
                rest = r;
            } else if let Some(r) = rest.strip_prefix("Alt+") {
                b.alt = true;
                rest = r;
            } else {
                break;
            }
        }
        b.key = Key::from_name(rest)?;
        Some(b)
    }

    /// Cómo se muestra en pantalla: «Ctrl+Mayús+S», «Espacio», «Supr».
    pub fn display(self) -> String {
        let key = match self.key {
            Key::Space => "Espacio".to_string(),
            Key::Escape => "Esc".to_string(),
            Key::Delete => "Supr".to_string(),
            Key::Backspace => "Retroceso".to_string(),
            Key::ArrowUp => "Flecha arriba".to_string(),
            Key::ArrowDown => "Flecha abajo".to_string(),
            Key::ArrowLeft => "Flecha izquierda".to_string(),
            Key::ArrowRight => "Flecha derecha".to_string(),
            Key::PageUp => "RePág".to_string(),
            Key::PageDown => "AvPág".to_string(),
            Key::Insert => "Insert".to_string(),
            Key::Home => "Inicio".to_string(),
            Key::End => "Fin".to_string(),
            k => k.symbol_or_name().to_string(),
        };
        format!("{}{}{}{key}", if self.ctrl { "Ctrl+" } else { "" }, if self.shift { "Mayús+" } else { "" }, if self.alt { "Alt+" } else { "" })
    }
}

/// Teclas que no se pueden asignar: las usa el salto a memoria «. N .» (punto y números) o son de Windows.
pub fn reserved(b: Binding) -> Option<&'static str> {
    let digit = matches!(b.key, Key::Num0 | Key::Num1 | Key::Num2 | Key::Num3 | Key::Num4 | Key::Num5 | Key::Num6 | Key::Num7 | Key::Num8 | Key::Num9);
    if !b.ctrl && !b.alt && (digit || b.key == Key::Period) {
        return Some("los números y el punto son del salto a memoria «. N .»");
    }
    if b.key == Key::Tab && !b.ctrl {
        return Some("Tab mueve el foco entre los campos");
    }
    if b.alt && b.key == Key::F4 {
        return Some("Alt+F4 cierra la ventana en Windows");
    }
    None
}

/// Las teclas de cada orden (hasta dos).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    map: BTreeMap<Command, [Option<Binding>; 2]>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self { map: Command::ALL.iter().map(|&c| (c, c.defaults())).collect() }
    }
}

impl Keymap {
    pub fn get(&self, c: Command) -> [Option<Binding>; 2] {
        self.map.get(&c).copied().unwrap_or_else(|| c.defaults())
    }

    /// La tecla principal para mostrar en botones y ayudas («M», «Ctrl+L»); «sin tecla» si no tiene.
    pub fn primary(&self, c: Command) -> String {
        self.get(c).iter().flatten().next().map_or_else(|| "sin tecla".to_string(), |b| b.display())
    }

    /// Todas las teclas de una orden, para la ayuda: «M / Enter».
    pub fn all_display(&self, c: Command) -> String {
        let v: Vec<String> = self.get(c).iter().flatten().map(|b| b.display()).collect();
        if v.is_empty() {
            "sin tecla".into()
        } else {
            v.join(" / ")
        }
    }

    /// ¿Qué otra orden usa ya esta tecla?
    pub fn owner(&self, b: Binding) -> Option<(Command, usize)> {
        self.map.iter().find_map(|(&c, slots)| slots.iter().position(|s| *s == Some(b)).map(|i| (c, i)))
    }

    /// Asigna `b` a la ranura `slot` de `c`. Rechaza teclas reservadas o ya usadas por otra orden.
    pub fn assign(&mut self, c: Command, slot: usize, b: Binding) -> Result<(), String> {
        if let Some(why) = reserved(b) {
            return Err(format!("{} no se puede usar: {why}", b.display()));
        }
        if let Some((other, i)) = self.owner(b) {
            if other != c || i != slot {
                return Err(format!("{} ya es la tecla de «{}»", b.display(), other.label()));
            }
        }
        let mut slots = self.get(c);
        slots[slot.min(1)] = Some(b);
        self.map.insert(c, slots);
        Ok(())
    }

    /// Quita una tecla. GO y PANIC no pueden quedar sin ninguna.
    pub fn clear(&mut self, c: Command, slot: usize) -> Result<(), String> {
        let mut slots = self.get(c);
        slots[slot.min(1)] = None;
        if c.required() && slots.iter().all(Option::is_none) {
            return Err(format!("«{}» necesita al menos una tecla", c.label()));
        }
        self.map.insert(c, slots);
        Ok(())
    }

    pub fn reset(&mut self, c: Command) {
        // Las de fábrica pueden chocar con una que el operador puso en otra orden: esa otra la pierde.
        for b in c.defaults().into_iter().flatten() {
            if let Some((other, i)) = self.owner(b) {
                if other != c {
                    let mut s = self.get(other);
                    s[i] = None;
                    self.map.insert(other, s);
                }
            }
        }
        self.map.insert(c, c.defaults());
        for o in Command::ALL {
            if o.required() && self.get(o).iter().all(Option::is_none) {
                self.map.insert(o, o.defaults());
            }
        }
    }

    pub fn reset_all(&mut self) {
        *self = Self::default();
    }

    /// ¿Esta pulsación es la orden `c`? PANIC responde aunque haya un modificador apretado (siempre tiene que funcionar).
    pub fn pressed(&self, c: Command, key: Key, m: Modifiers, repeat: bool) -> bool {
        if repeat && !c.repeats() {
            return false;
        }
        self.get(c).iter().flatten().any(|b| if c == Command::Panic && !b.ctrl && !b.shift && !b.alt { b.key == key } else { b.matches(key, m) })
    }
}

/// Las preferencias que se guardan.
#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    /// Ensayo: al parar (Esc, «Parar todo»), la música queda lista para retomar desde ese punto con el próximo GO.
    pub resume_after_stop: bool,
    /// Silenciar los sonidos del sistema de Windows mientras TeatroCue está abierto.
    pub mute_system_sounds: bool,
    pub keys: Keymap,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { resume_after_stop: false, mute_system_sounds: true, keys: Keymap::default() }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct OnDisk {
    resume_after_stop: Option<bool>,
    mute_system_sounds: Option<bool>,
    /// Orden → teclas («Ctrl+L»). Lo que falta o no se entiende queda de fábrica.
    keys: BTreeMap<Command, Vec<String>>,
}

impl Prefs {
    fn to_disk(&self) -> OnDisk {
        OnDisk {
            resume_after_stop: Some(self.resume_after_stop),
            mute_system_sounds: Some(self.mute_system_sounds),
            keys: Command::ALL.iter().map(|&c| (c, self.keys.get(c).iter().flatten().map(|b| b.to_config()).collect())).collect(),
        }
    }

    fn from_disk(d: OnDisk) -> Self {
        let def = Prefs::default();
        let mut keys = Keymap::default();
        for (c, list) in d.keys {
            let mut slots = [None, None];
            for (i, s) in list.iter().take(2).enumerate() {
                slots[i] = Binding::from_config(s);
            }
            if c.required() && slots.iter().all(Option::is_none) {
                continue;
            }
            keys.map.insert(c, slots);
        }
        // Un archivo editado a mano con dos órdenes en la misma tecla: gana la primera, la otra queda sin esa tecla.
        let mut seen = std::collections::HashSet::new();
        for c in Command::ALL {
            let mut s = keys.get(c);
            for slot in s.iter_mut() {
                if let Some(b) = *slot {
                    if !seen.insert(b.to_config()) || reserved(b).is_some() {
                        *slot = None;
                    }
                }
            }
            if c.required() && s.iter().all(Option::is_none) {
                s = c.defaults();
            }
            keys.map.insert(c, s);
        }
        Self { resume_after_stop: d.resume_after_stop.unwrap_or(def.resume_after_stop), mute_system_sounds: d.mute_system_sounds.unwrap_or(def.mute_system_sounds), keys }
    }
}

/// `preferencias.json` junto a `audio.json`.
pub fn path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name("preferencias.json")
}

/// Lee las preferencias; si faltan o están dañadas, las de fábrica (nunca falla).
pub fn load(path: &Path) -> Prefs {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<OnDisk>(&t).ok()).map(Prefs::from_disk).unwrap_or_default()
}

/// Guarda de forma atómica (temporal + renombrado).
pub fn save(path: &Path, p: &Prefs) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&p.to_disk()).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// La primera tecla (no modificadora) que se pulsó en esta pasada, para capturar un atajo nuevo.
pub fn captured(i: &egui::InputState) -> Option<Binding> {
    i.events.iter().find_map(|e| match e {
        egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. } => Some(Binding::from_event(*key, *modifiers)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers::COMMAND;

    #[test]
    fn the_factory_keys_are_the_ones_the_manual_documents() {
        let k = Keymap::default();
        assert!(k.pressed(Command::Go, Key::Space, NONE, false));
        assert!(!k.pressed(Command::Go, Key::Space, NONE, true), "mantener Espacio no dispara una cue tras otra");
        assert!(!k.pressed(Command::Go, Key::Space, Modifiers::SHIFT, false));
        assert!(k.pressed(Command::Panic, Key::Escape, Modifiers::SHIFT, false), "PANIC funciona aunque haya un modificador apretado");
        assert!(k.pressed(Command::Memory, Key::Enter, NONE, false) && k.pressed(Command::Memory, Key::M, NONE, false));
        assert!(k.pressed(Command::Redo, Key::Y, CTRL, false) && k.pressed(Command::Redo, Key::Z, Modifiers::COMMAND | Modifiers::SHIFT, false));
        assert!(!k.pressed(Command::Undo, Key::Z, Modifiers::COMMAND | Modifiers::SHIFT, false));
        assert!(k.pressed(Command::Down, Key::ArrowDown, NONE, true), "las flechas sí repiten");
        assert_eq!(k.primary(Command::Lock), "Ctrl+L");
        assert_eq!(k.all_display(Command::Memory), "M / Enter");
        // Ninguna tecla está repetida entre órdenes.
        let mut all: Vec<String> = Command::ALL.iter().flat_map(|&c| k.get(c).into_iter().flatten().map(|b| b.to_config())).collect();
        let n = all.len();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), n);
    }

    #[test]
    fn a_new_key_is_refused_if_another_command_uses_it_or_it_is_reserved_and_go_and_panic_never_end_up_without_a_key() {
        let mut k = Keymap::default();
        let err = k.assign(Command::Pause, 0, Binding::plain(Key::Space)).unwrap_err();
        assert!(err.contains("GO"), "{err}");
        assert!(k.assign(Command::Pause, 0, Binding::plain(Key::Num5)).unwrap_err().contains("«. N .»"));
        k.assign(Command::Go, 0, Binding::plain(Key::F12)).unwrap();
        assert!(k.pressed(Command::Go, Key::F12, NONE, false) && !k.pressed(Command::Go, Key::Space, NONE, false));
        // Ahora Espacio está libre.
        k.assign(Command::Pause, 0, Binding::plain(Key::Space)).unwrap();
        assert!(k.clear(Command::Go, 0).unwrap_err().contains("al menos una tecla"));
        k.clear(Command::Memory, 1).unwrap();
        assert!(!k.pressed(Command::Memory, Key::Enter, NONE, false));
        // Restaurar GO: Espacio vuelve a GO y Pausa la pierde (no quedan dos órdenes en la misma tecla).
        k.reset(Command::Go);
        assert!(k.pressed(Command::Go, Key::Space, NONE, false) && !k.pressed(Command::Pause, Key::Space, NONE, false));
        k.reset_all();
        assert_eq!(k, Keymap::default());
    }

    #[test]
    fn preferences_round_trip_and_a_damaged_or_hand_edited_file_never_breaks_go_or_panic() {
        let dir = std::env::temp_dir().join(format!("qlab-prefs-{}", std::process::id()));
        let p = path(&dir.join("audio.json"));
        assert_eq!(load(&p), Prefs::default(), "sin archivo: de fábrica");
        let mut prefs = Prefs { resume_after_stop: true, mute_system_sounds: false, ..Default::default() };
        prefs.keys.assign(Command::Pause, 0, Binding { key: Key::P, ctrl: false, shift: true, alt: false }).unwrap();
        save(&p, &prefs).unwrap();
        assert_eq!(load(&p), prefs);
        std::fs::write(&p, r#"{"keys":{"Go":[],"Panic":["Teclazo"],"Pause":["Space"],"Help":["Space"]}}"#).unwrap();
        let l = load(&p);
        assert!(l.keys.pressed(Command::Go, Key::Space, NONE, false) && l.keys.pressed(Command::Panic, Key::Escape, NONE, false));
        assert!(!l.keys.pressed(Command::Pause, Key::Space, NONE, false), "Espacio es de GO aunque el archivo diga otra cosa");
        assert!(l.mute_system_sounds && !l.resume_after_stop, "lo que falta queda de fábrica");
        std::fs::write(&p, "esto no es json").unwrap();
        assert_eq!(load(&p), Prefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bindings_read_back_what_they_write_and_show_spanish_names() {
        for b in [Binding::plain(Key::Space), Binding { key: Key::S, ctrl: true, shift: true, alt: false }, Binding { key: Key::F5, ctrl: false, shift: false, alt: true }] {
            assert_eq!(Binding::from_config(&b.to_config()), Some(b));
        }
        assert_eq!(Binding { key: Key::S, ctrl: true, shift: true, alt: false }.display(), "Ctrl+Mayús+S");
        assert_eq!(Binding::plain(Key::Delete).display(), "Supr");
        assert_eq!(Binding::from_config("nada"), None);
    }
}

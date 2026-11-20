use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

static RUSSIAN: AtomicBool = AtomicBool::new(false);

fn is_russian_id(id: u16) -> bool {
    id & 0x03ff == 0x19
}

pub fn is_russian() -> bool {
    RUSSIAN.load(Ordering::Relaxed)
}

pub fn load(dir: &Path) {
    let value = fs::read_to_string(dir.join("language.txt")).unwrap_or_default();
    let russian = match value.trim() {
        "ru" => true,
        "en" => false,
        _ => is_russian_id(unsafe { GetUserDefaultUILanguage() }),
    };
    RUSSIAN.store(russian, Ordering::Relaxed);
}

pub fn toggle(dir: &Path) -> io::Result<()> {
    let russian = !is_russian();
    fs::create_dir_all(dir)?;
    crate::platform::persist(
        &dir.join("language.txt"),
        if russian { "ru\n" } else { "en\n" },
    )?;
    RUSSIAN.store(russian, Ordering::Relaxed);
    Ok(())
}

pub fn tr<'a>(russian: &'a str, english: &'a str) -> &'a str {
    if is_russian() { russian } else { english }
}

macro_rules! trf {
    ($russian:literal, $english:literal $(, $value:expr)* $(,)?) => {
        if $crate::locale::is_russian() {
            format!($russian $(, $value)*)
        } else {
            format!($english $(, $value)*)
        }
    };
}

pub(crate) use trf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_russian_by_primary_ui_language() {
        assert!(is_russian_id(0x0419));
        assert!(is_russian_id(0x0819));
        assert!(!is_russian_id(0x0409));
        assert!(!is_russian_id(0x0809));
        assert!(!is_russian_id(0x0407));
    }
}

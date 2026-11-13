//! Spanish and English UI text. The language follows the system unless chosen in Ajustes › General.
//!
//! Strings stay inline as Spanish/English pairs (`tr!`), so there are no keys to keep in sync.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Language {
    Spanish,
    English,
    Russian,
}

impl Language {
    /// The system display language: Spanish for any Spanish variant, English otherwise.
    pub fn system() -> Self {
        crate::platform::sys::system_language()
    }

    /// From a POSIX locale such as "es_ES.UTF-8".
    #[cfg_attr(windows, allow(dead_code))]
    pub fn from_locale(locale: &str) -> Self {
        if locale == "ru" || locale.starts_with("ru_") || locale.starts_with("ru.") || locale.starts_with("ru@") {
            Self::Russian
        } else if locale == "es" || locale.starts_with("es_") || locale.starts_with("es.") || locale.starts_with("es@") {
            Self::Spanish
        } else {
            Self::English
        }
    }

    /// From a Windows language id.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn from_lang_id(id: u16) -> Self {
        const LANG_SPANISH: u16 = 0x0A;
        match id & 0x3FF { 0x19 => Self::Russian, LANG_SPANISH => Self::Spanish, _ => Self::English }
    }

    /// The language's own name, so it is recognizable whatever the current language.
    pub fn name(self) -> &'static str {
        match self {
            Self::Spanish => "Español",
            Self::English => "English",
            Self::Russian => "Русский",
        }
    }
}

pub fn set(language: Language) {
    LANGUAGE.store(match language { Language::Spanish => 0, Language::English => 1, Language::Russian => 2 }, Ordering::Relaxed);
}

pub fn english() -> bool {
    LANGUAGE.load(Ordering::Relaxed) != 0
}

pub fn russian() -> bool {
    LANGUAGE.load(Ordering::Relaxed) == 2
}

pub fn translated(spanish: &'static str, english: &'static str) -> &'static str {
    if LANGUAGE.load(Ordering::Relaxed) == 0 { return spanish; }
    if !russian() { return english; }
    match english {
        "Settings" => "Настройки", "General" => "Общие", "Output" => "Запись", "Audio" => "Звук",
        "Video" => "Видео", "Hotkeys" => "Горячие клавиши", "Language" => "Язык",
        "Automatic (system language)" => "Как в системе", "System Tray" => "Системный трей",
        "Minimize to the system tray" => "Сворачивать в трей", "Capture" => "Захват",
        "Hide opnoverlay from screen capture" => "Не показывать opnoverlay на записи",
        "Recording" => "Запись", "Recording Path" => "Папка записей", "Browse" => "Выбрать",
        "Recording Quality" => "Качество записи", "Recording Format" => "Формат записи",
        "Encoder" => "Кодировщик", "Hardware H.264 (software if the GPU has none)" => "Аппаратный H.264; иначе программный",
        "Replay Buffer" => "Откаты", "Start automatically" => "Включать при запуске",
        "Global Audio Devices" => "Устройства звука", "Desktop Audio" => "Звук рабочего стола",
        "Mic/Auxiliary Audio" => "Микрофон", "Microphone noise suppression (RNNoise)" => "Шумоподавление микрофона",
        "Output (Scaled) Resolution" => "Разрешение записи", "Global Hotkeys" => "Горячие клавиши",
        "Open / close menu" => "Открыть / закрыть меню", "Start / Stop Recording" => "Начать / остановить запись",
        "Pause / Resume Recording" => "Пауза / продолжить запись", "Start / Stop Replay Buffer" => "Включить / выключить откаты",
        "Save Replay" => "Сохранить откат", "Take Screenshot" => "Сделать скриншот", "Mute / Unmute Mic/Aux" => "Включить / выключить микрофон",
        "OK" => "Сохранить", "Cancel" => "Отмена", "Close" => "Закрыть", "Exit" => "Выход",
        "Start Recording" => "Начать запись", "Stop Recording" => "Остановить запись",
        "Pause Recording" => "Приостановить запись", "Resume Recording" => "Продолжить запись",
        "Start Replay Buffer" => "Включить откаты", "Stop Replay Buffer" => "Выключить откаты",
        "Replay buffer active" => "Откаты включены", "Starting…" => "Запуск…", "Saving…" => "Сохранение…",
        "Saving recording…" => "Сохранение записи…", "Screenshot" => "Скриншот", "Show Recordings" => "Открыть папку записей",
        "Show in folder" => "Показать в папке", "Default" => "По умолчанию", "Disabled" => "Выключено",
        "Sources" => "Источники", "Audio Mixer" => "Микшер звука", "Controls" => "Управление",
        "Display Capture" => "Захват экрана", "Window Capture" => "Захват окна", "Capture Cursor" => "Записывать курсор",
        "Properties" => "Свойства", "Clear" => "Очистить", "Mute" => "Выключить звук", "Unmute" => "Включить звук",
        "Native" => "Исходное",
        "Camera" => "Камера", "No camera found" => "Камера не найдена", "Press a combination…" => "Нажмите сочетание…", "(none)" => "Не назначено",
        "Application Audio Capture" => "Захват звука приложения", "Video Capture Device" => "Камера",
        "Add source" => "Добавить источник", "Select Region…" => "Выбрать область…",
        "Record Whole Source" => "Записывать весь источник", "Remove Application Audio" => "Убрать звук приложения",
        "Remove Camera" => "Убрать камеру", "region" => "область", " (not running)" => " (не запущено)",
        "Top left" => "Сверху слева", "Top right" => "Сверху справа", "Bottom left" => "Снизу слева", "Bottom right" => "Снизу справа",
        "Small" => "Маленький", "Medium" => "Средний", "Large" => "Большой",
        "Start the replay buffer to save replays." => "Включите откаты, чтобы их сохранять.",
        "Close Settings to start." => "Закройте настройки перед запуском.",
        "Stop recording and the replay buffer to change these settings." => "Остановите запись, чтобы изменить настройки.",
        "They work even when obs-lite is minimized or in the background." => "Работают, когда opnoverlay свёрнут или находится в фоне.",
        "Show opnoverlay" => "Открыть opnoverlay", "opnoverlay · Paused" => "opnoverlay · Пауза",
        "opnoverlay · Recording" => "opnoverlay · Запись", "opnoverlay · Replay Buffer" => "opnoverlay · Откаты",
        _ => english,
    }
}

/// `tr!("es", "en")` picks a string; with arguments it formats the chosen one.
macro_rules! tr {
    ($es:literal, $en:literal $(,)?) => {
        $crate::i18n::translated($es, $en)
    };
    ($es:literal, $en:literal, $($arg:expr),+ $(,)?) => {
        if $crate::i18n::english() { format!($en, $($arg),+) } else { format!($es, $($arg),+) }
    };
}
pub(crate) use tr;

macro_rules! tr3 {
    ($es:literal, $en:literal, $ru:literal $(,)?) => {
        if $crate::i18n::russian() { $ru } else { $crate::i18n::tr!($es, $en) }
    };
    ($es:literal, $en:literal, $ru:literal, $($arg:expr),+ $(,)?) => {
        if $crate::i18n::russian() { format!($ru, $($arg),+) } else { $crate::i18n::tr!($es, $en, $($arg),+) }
    };
}
pub(crate) use tr3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spanish_variant_is_spanish() {
        assert_eq!(Language::from_lang_id(0x0C0A), Language::Spanish); // es-ES
        assert_eq!(Language::from_lang_id(0x080A), Language::Spanish); // es-MX
        assert_eq!(Language::from_lang_id(0x0409), Language::English); // en-US
        assert_eq!(Language::from_lang_id(0x040C), Language::English); // fr-FR
    }

    #[test]
    fn every_spanish_locale_is_spanish() {
        for locale in ["es_ES.UTF-8", "es_MX", "es", "es.UTF-8", "es_ES@euro"] {
            assert_eq!(Language::from_locale(locale), Language::Spanish, "{locale}");
        }
        for locale in ["en_US.UTF-8", "C", "POSIX", "", "eu_ES.UTF-8", "esperanto"] {
            assert_eq!(Language::from_locale(locale), Language::English, "{locale}");
        }
    }

    #[test]
    fn russian_is_detected_from_system_language() {
        assert_eq!(Language::from_lang_id(0x0419), Language::Russian);
        assert_eq!(Language::from_locale("ru_RU.UTF-8"), Language::Russian);
        assert_eq!(Language::from_locale("ru"), Language::Russian);
    }
}

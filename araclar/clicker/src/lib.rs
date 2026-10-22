//! clicker: otomatik tıklayıcı. İstediğin kadar tetik tuşu; her tuşun kendi eylemi (sol / sağ /
//! orta tık, tekerlek yukarı / aşağı ya da bir klavye tuşu), kendi modu (basılı tut ya da
//! aç / kapa), hızı ve sayısı. Tuşa basılı tutunca (ya da bir kez basınca) imlecin olduğu yere
//! eylemi seçilen hızda yineler; istenirse belli sayıdan sonra kendiliğinden durur. Tuşlar
//! birbirinden bağımsızdır: biri sürerken öbürü de başlayabilir.
//!
//! Tetik tuşları RegisterHotKey ile alınır: klavyedeki her tuş (değiştiricilerle birlikte) olur,
//! fare düğmesi olmaz. Basılı tutma modunda tuşun bırakıldığını eylemin iş parçacığı izler.
//! Eylemler ayrı bir iş parçacığında SendInput ile gider: zamanlaması arayüzden bağımsız,
//! sürüklenmez (her yineleme başlangıca göre hesaplanır).

#[macro_use]
extern crate ortak;

mod tool;
pub use tool::TOOL;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ortak::hotkey::{self, Hotkey};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use ortak::form::{Ctl, Form, Hit, Item, Row};
use ortak::gfx::{Gfx, Rect};
use ortak::osd;
use ortak::ui::*;
use ortak::util;

/// Bir eylem bitti (eylemin iş parçacığından): wParam çalıştırma kimliği, lParam neden
/// (0 tuş bırakıldı, 1 sayı doldu, 2 gönderilemedi).
pub const WM_DONE: u32 = WM_APP + 60;
/// Tetik tuşlarının ilk kısayol kimliği (soundboard 1001.., audio 5001..); i. tuş HK_TRIGGER + i.
pub const HK_TRIGGER: i32 = 6001;
/// Eylem sürerken sayaç ekranda tazelenir.
pub const TIMER_UI: usize = 602;

const UI_MS: u32 = 100;
const MAX_TRIGGERS: usize = 32;
const ICON_MOUSE: &str = "\u{E962}";
/// Aracın rengi (ikonuyla aynı), bildirimde.
const COLOR: u32 = 0xA3E635;

/// Hız kaydırıcısı: saniyede yineleme, logaritmik.
const CPS_MIN: f32 = 0.5;
const CPS_MAX: f32 = 100.0;
/// Sayı kaydırıcısı: en solda sınırsız, gerisi 1..LIMIT_MAX logaritmik.
const LIMIT_MAX: f32 = 10000.0;
const LIMIT_OFF: f32 = 0.05;

const DEFAULT_KEY: &str = "F6";
const DEFAULT_CPS: f32 = 10.0;
/// Tekerleğin bir çentiği.
const WHEEL_STEP: i32 = 120;

pub fn dir() -> PathBuf {
    util::data_dir().join("clicker")
}

fn ini() -> PathBuf {
    dir().join("ayarlar.ini")
}

pub fn installed() -> bool {
    dir().is_dir()
}

// --- Ayarlar ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Tuş basılıyken yineler.
    Hold,
    /// Bir basış başlatır, bir basış durdurur.
    Toggle,
}

impl Mode {
    const ALL: [Mode; 2] = [Mode::Hold, Mode::Toggle];

    fn id(self) -> &'static str {
        match self {
            Mode::Hold => "basili",
            Mode::Toggle => "ackapa",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Hold => t!("Hold", "Basılı tut"),
            Mode::Toggle => t!("Toggle", "Aç / kapa"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    const ALL: [Button; 3] = [Button::Left, Button::Right, Button::Middle];

    fn id(self) -> &'static str {
        match self {
            Button::Left => "sol",
            Button::Right => "sag",
            Button::Middle => "orta",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Button::Left => t!("Left", "Sol"),
            Button::Right => t!("Right", "Sağ"),
            Button::Middle => t!("Middle", "Orta"),
        }
    }

    fn flags(self) -> (MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS) {
        match self {
            Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }
    }
}

/// Eylemin türü (sayfadaki seçenekler).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Click,
    Wheel,
    Key,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Click, Kind::Wheel, Kind::Key];

    fn label(self) -> &'static str {
        match self {
            Kind::Click => t!("Click", "Tıklama"),
            Kind::Wheel => t!("Wheel", "Tekerlek"),
            Kind::Key => t!("Keyboard", "Klavye"),
        }
    }
}

/// Tetik tuşunun yinelediği şey.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Click(Button),
    /// `true`: yukarı.
    Wheel(bool),
    /// Basılacak klavye tuşu (değiştiricileriyle); henüz seçilmediyse None.
    Key(Option<Hotkey>),
}

impl Action {
    fn kind(self) -> Kind {
        match self {
            Action::Click(_) => Kind::Click,
            Action::Wheel(_) => Kind::Wheel,
            Action::Key(_) => Kind::Key,
        }
    }

    /// Tür değişince o türün başlangıcı; aynı türse olduğu gibi.
    fn with_kind(self, kind: Kind) -> Action {
        if self.kind() == kind {
            return self;
        }
        match kind {
            Kind::Click => Action::Click(Button::Left),
            Kind::Wheel => Action::Wheel(true),
            Kind::Key => Action::Key(None),
        }
    }

    /// Çalıştırılabilir mi (klavye eyleminde tuş seçilmiş mi).
    fn ready(self) -> bool {
        !matches!(self, Action::Key(None))
    }

    fn id(self) -> String {
        match self {
            Action::Click(b) => b.id().into(),
            Action::Wheel(true) => "teker+".into(),
            Action::Wheel(false) => "teker-".into(),
            Action::Key(k) => format!("tus:{}", k.map(|k| k.to_string()).unwrap_or_default()),
        }
    }

    fn parse(s: &str) -> Option<Action> {
        if let Some(key) = s.strip_prefix("tus:") {
            return Some(Action::Key(Hotkey::parse(key)));
        }
        match s {
            "teker+" => Some(Action::Wheel(true)),
            "teker-" => Some(Action::Wheel(false)),
            _ => Button::ALL.into_iter().find(|b| b.id() == s).map(Action::Click),
        }
    }

    fn label(self) -> String {
        match self {
            Action::Click(Button::Left) => t!("Left click", "Sol tık").into(),
            Action::Click(Button::Right) => t!("Right click", "Sağ tık").into(),
            Action::Click(Button::Middle) => t!("Middle click", "Orta tık").into(),
            Action::Wheel(true) => t!("Wheel up", "Tekerlek ↑").into(),
            Action::Wheel(false) => t!("Wheel down", "Tekerlek ↓").into(),
            Action::Key(Some(k)) => t!(format!("Key {k}"), format!("Tuş {k}")),
            Action::Key(None) => t!("Key (not picked)", "Tuş (seçilmedi)").into(),
        }
    }
}

/// Bir tetik tuşu ve ne yapacağı.
#[derive(Clone, PartialEq, Debug)]
struct Trigger {
    key: Hotkey,
    action: Action,
    mode: Mode,
    /// Saniyede yineleme.
    cps: f32,
    /// Bu kadar yinelemeden sonra durur; 0 sınırsız.
    limit: u32,
}

impl Trigger {
    fn new(key: Hotkey) -> Self {
        Trigger { key, action: Action::Click(Button::Left), mode: Mode::Hold, cps: DEFAULT_CPS, limit: 0 }
    }

    /// Dosyadaki hali: `F6|sol|basili|10|0`.
    fn line(&self) -> String {
        format!("{}|{}|{}|{}|{}", self.key, self.action.id(), self.mode.id(), self.cps, self.limit)
    }

    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split('|').map(str::trim);
        let key = Hotkey::parse(parts.next()?)?;
        let mut t = Trigger::new(key);
        if let Some(a) = parts.next().and_then(Action::parse) {
            t.action = a;
        }
        if let Some(m) = parts.next().and_then(|v| Mode::ALL.into_iter().find(|m| m.id() == v)) {
            t.mode = m;
        }
        if let Some(c) = parts.next().and_then(|v| v.parse::<f32>().ok()) {
            t.cps = c.clamp(CPS_MIN, CPS_MAX);
        }
        if let Some(n) = parts.next().and_then(|v| v.parse::<u32>().ok()) {
            t.limit = n.min(LIMIT_MAX as u32);
        }
        Some(t)
    }
}

#[derive(Clone)]
struct Settings {
    /// Kapalıyken tetik tuşları alınmaz; ayarlar durur.
    enabled: bool,
    /// Dosyada hiç yoksa F6 ile sol tık; boş liste tetikleyiciyi kapatır.
    triggers: Vec<Trigger>,
}

impl Settings {
    fn load() -> Self {
        Self::parse(&std::fs::read_to_string(ini()).unwrap_or_default())
    }

    /// `surum=2` ile `tetik=` satırları yeni biçim. Eski biçim (`tus`, `tuslar`, `mod`, `dugme`,
    /// `hiz`, `sayi`) her tuşu aynı ayarlarla birer tetik yapar.
    fn parse(text: &str) -> Self {
        let mut s = Settings { enabled: true, triggers: Vec::new() };
        let mut new_format = false;
        let mut legacy_seen = false;
        let mut legacy_keys: Vec<Hotkey> = Vec::new();
        let mut legacy_list = false;
        let mut legacy = Trigger::new(Hotkey { mods: HOT_KEY_MODIFIERS(0), vk: 0 });
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "acik" => s.enabled = v != "0",
                "surum" => new_format = true,
                "tetik" => {
                    if let Some(t) = Trigger::parse(v)
                        && !s.triggers.iter().any(|x| x.key == t.key)
                        && s.triggers.len() < MAX_TRIGGERS
                    {
                        s.triggers.push(t);
                    }
                }
                "tus" => {
                    legacy_seen = true;
                    if !legacy_list {
                        legacy_keys = Hotkey::parse(v).into_iter().collect();
                    }
                }
                "tuslar" => {
                    legacy_seen = true;
                    legacy_list = true;
                    legacy_keys.clear();
                    for key in v.split(',').filter_map(Hotkey::parse) {
                        if !legacy_keys.contains(&key) && legacy_keys.len() < MAX_TRIGGERS {
                            legacy_keys.push(key);
                        }
                    }
                }
                "mod" => legacy.mode = Mode::ALL.into_iter().find(|m| m.id() == v).unwrap_or(legacy.mode),
                "dugme" => {
                    legacy.action =
                        Button::ALL.into_iter().find(|b| b.id() == v).map(Action::Click).unwrap_or(legacy.action)
                }
                "hiz" => legacy.cps = v.parse().map_or(legacy.cps, |c: f32| c.clamp(CPS_MIN, CPS_MAX)),
                "sayi" => legacy.limit = v.parse().unwrap_or(legacy.limit),
                _ => {}
            }
        }
        if !new_format {
            if !legacy_seen {
                legacy_keys = Hotkey::parse(DEFAULT_KEY).into_iter().collect();
            }
            s.triggers = legacy_keys.into_iter().map(|key| Trigger { key, ..legacy.clone() }).collect();
        }
        s
    }

    fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(dir())?;
        let mut text = format!("acik={}\nsurum=2\n", self.enabled as u8);
        for t in &self.triggers {
            text.push_str("tetik=");
            text.push_str(&t.line());
            text.push('\n');
        }
        std::fs::write(ini(), text)
    }
}

pub fn install() -> Result<(), String> {
    Settings::load().save().map_err(|e| format!("{}: {e}", ini().display()))
}

pub fn uninstall() -> Result<(), String> {
    match std::fs::remove_dir_all(dir()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{} {}: {e}", t!("could not delete", "silinemedi:"), dir().display()))
        }
        _ => Ok(()),
    }
}

pub fn leftovers() -> Vec<String> {
    let d = dir();
    d.exists().then(|| d.display().to_string()).into_iter().collect()
}

// --- Girdi ---

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Uygulamaların ayırt ettiği genişletilmiş tuşlar (oklar, Ins / Del, sağ Ctrl...).
fn extended(vk: u16) -> bool {
    matches!(
        VIRTUAL_KEY(vk),
        VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_DIVIDE
            | VK_NUMLOCK
            | VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_APPS
            | VK_SNAPSHOT
    )
}

fn key_input(vk: u16, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    let mut flags = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
    if extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

/// Kısayolun değiştiricilerinin sanal tuşları (basılacak sırayla).
fn modifier_keys(k: Hotkey) -> Vec<u16> {
    [(MOD_CONTROL, VK_CONTROL), (MOD_ALT, VK_MENU), (MOD_SHIFT, VK_SHIFT), (MOD_WIN, VK_LWIN)]
        .into_iter()
        .filter(|(m, _)| k.mods.contains(*m))
        .map(|(_, v)| v.0)
        .collect()
}

/// Eylemin bir yinelemesi: basış ve bırakış (ya da tekerleğin bir çentiği).
fn inputs(action: Action) -> Vec<INPUT> {
    match action {
        Action::Click(b) => {
            let (down, up) = b.flags();
            vec![mouse_input(down, 0), mouse_input(up, 0)]
        }
        Action::Wheel(up) => vec![mouse_input(MOUSEEVENTF_WHEEL, if up { WHEEL_STEP } else { -WHEEL_STEP })],
        Action::Key(None) => Vec::new(),
        Action::Key(Some(k)) => {
            let mods = modifier_keys(k);
            let mut v: Vec<INPUT> = mods.iter().map(|&m| key_input(m, false)).collect();
            v.push(key_input(k.vk, false));
            v.push(key_input(k.vk, true));
            v.extend(mods.iter().rev().map(|&m| key_input(m, true)));
            v
        }
    }
}

/// Yarım kalan basışı bırakan girdiler.
fn releases(action: Action) -> Vec<INPUT> {
    match action {
        Action::Click(b) => vec![mouse_input(b.flags().1, 0)],
        Action::Wheel(_) | Action::Key(None) => Vec::new(),
        Action::Key(Some(k)) => {
            let mut v = vec![key_input(k.vk, true)];
            v.extend(modifier_keys(k).iter().rev().map(|&m| key_input(m, true)));
            v
        }
    }
}

/// İmlecin olduğu yere eylemin bir yinelemesi; gidemediyse `false`.
fn perform(action: Action) -> bool {
    let batch = inputs(action);
    if batch.is_empty() {
        return false;
    }
    let sent = unsafe { SendInput(&batch, size_of::<INPUT>() as i32) };
    if sent as usize == batch.len() {
        return true;
    }
    if sent > 0 {
        let rel = releases(action);
        if !rel.is_empty() {
            unsafe {
                let _ = SendInput(&rel, size_of::<INPUT>() as i32);
            }
        }
    }
    false
}

fn key_down(key: Hotkey) -> bool {
    let down = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0;
    down(VIRTUAL_KEY(key.vk))
        && (!key.mods.contains(MOD_CONTROL) || down(VK_CONTROL))
        && (!key.mods.contains(MOD_ALT) || down(VK_MENU))
        && (!key.mods.contains(MOD_SHIFT) || down(VK_SHIFT))
        && (!key.mods.contains(MOD_WIN) || down(VK_LWIN) || down(VK_RWIN))
}

// --- Çalıştırma ---

/// Süren bir eylem: hangi tetik, durdurma bayrağı ve sayaç. Düşürülünce iş parçacığı biter.
struct Run {
    id: u32,
    trigger: usize,
    stop: Arc<AtomicBool>,
    count: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Run {
    fn count(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }

    /// İş parçacığı kendiliğinden bitti (tuş bırakıldı, sayı doldu) ama WM_DONE daha gelmedi.
    fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Eylemi ayrı iş parçacığında başlatır; sayaç `from`'dan sürer (hız değişince yeniden
/// başlatılır). Bitince pencereye WM_DONE gelir.
fn start_run(t: &Trigger, trigger: usize, hwnd: usize, from: u32, id: u32) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU32::new(from));
    let (worker_stop, worker_count) = (stop.clone(), count.clone());
    let (action, limit) = (t.action, t.limit);
    let hold_key = (t.mode == Mode::Hold).then_some(t.key);
    let period = Duration::from_secs_f32(1.0 / t.cps.clamp(CPS_MIN, CPS_MAX));
    let done = move |reason: isize| unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_DONE, WPARAM(id as usize), LPARAM(reason));
    };
    let thread = std::thread::spawn(move || {
        // Sleep 15 ms'ye kadar kaba olabilir: bu iş parçacığı sürerken 1 ms'ye iner.
        unsafe {
            let _ = timeBeginPeriod(1);
        }
        let released = || hold_key.is_some_and(|k| !key_down(k));
        let start = Instant::now();
        let mut i = 0u32;
        while !worker_stop.load(Ordering::Relaxed) {
            if i > 0 && released() {
                done(0);
                break;
            }
            if !perform(action) {
                done(2);
                break;
            }
            i += 1;
            let n = from + i;
            worker_count.store(n, Ordering::Relaxed);
            if limit > 0 && n >= limit {
                done(1);
                break;
            }
            // Bir sonraki yineleme başlangıca göre: aralıklar birikmez. Son milisaniye beklenmez.
            let next = start + period * i;
            loop {
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                if released() {
                    done(0);
                    break;
                }
                let now = Instant::now();
                if now >= next {
                    break;
                }
                let left = next - now;
                if left > Duration::from_millis(2) {
                    std::thread::park_timeout((left - Duration::from_millis(1)).min(Duration::from_millis(10)));
                } else {
                    std::thread::yield_now();
                }
            }
        }
        unsafe {
            let _ = timeEndPeriod(1);
        }
    });
    Run { id, trigger, stop, count, thread: Some(thread) }
}

// --- Kaydırıcılar ---

fn cps_to_v(cps: f32) -> f32 {
    ((cps / CPS_MIN).ln() / (CPS_MAX / CPS_MIN).ln()).clamp(0.0, 1.0)
}

/// Kaydırıcıdan hız: hızlıda tam sayı, ortada yarım, yavaşta onda bir adımlarla.
fn v_to_cps(v: f32) -> f32 {
    quant_cps(CPS_MIN * (CPS_MAX / CPS_MIN).powf(v.clamp(0.0, 1.0)))
}

fn quant_cps(c: f32) -> f32 {
    let q = if c >= 10.0 {
        c.round()
    } else if c >= 1.0 {
        (c * 2.0).round() / 2.0
    } else {
        (c * 10.0).round() / 10.0
    };
    q.clamp(CPS_MIN, CPS_MAX)
}

fn limit_to_v(n: u32) -> f32 {
    if n == 0 { 0.0 } else { LIMIT_OFF + (n as f32).log10() / LIMIT_MAX.log10() * (1.0 - LIMIT_OFF) }
}

fn v_to_limit(v: f32) -> u32 {
    if v < LIMIT_OFF {
        return 0;
    }
    let n = 10f32.powf((v.min(1.0) - LIMIT_OFF) / (1.0 - LIMIT_OFF) * LIMIT_MAX.log10());
    quant_limit(n)
}

/// Yuvarlak sayılar: 20'ye kadar birer, 200'e kadar beşer, 2000'e kadar ellişer, sonra beş yüzer.
fn quant_limit(n: f32) -> u32 {
    let step = if n < 20.0 {
        1.0
    } else if n < 200.0 {
        5.0
    } else if n < 2000.0 {
        50.0
    } else {
        500.0
    };
    ((n / step).round() * step).clamp(1.0, LIMIT_MAX) as u32
}

fn cps_step(c: f32) -> f32 {
    if c >= 10.0 {
        1.0
    } else if c >= 1.0 {
        0.5
    } else {
        0.1
    }
}

fn limit_step(n: u32) -> u32 {
    if n < 20 {
        1
    } else if n < 200 {
        5
    } else if n < 2000 {
        50
    } else {
        500
    }
}

fn fmt_cps(cps: f32) -> String {
    let trim = |v: f32| {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
    };
    if cps < 1.0 {
        t!(format!("every {} s", trim(1.0 / cps)), format!("{} sn'de bir", trim(1.0 / cps)))
    } else {
        t!(format!("{} / s", trim(cps)), format!("{} / sn", trim(cps)))
    }
}

fn fmt_limit(n: u32) -> String {
    if n == 0 { "∞".into() } else { t!(format!("{n} times"), format!("{n} kez")) }
}

// --- Sayfa ---

/// Sayfa kitindeki satırların kimlikleri: genel satırlar ve her tetiğin R_STRIDE'lık aralığı.
const R_ON: u32 = 0;
const R_ADD: u32 = 1;
const R_BASE: u32 = 100;
const R_STRIDE: u32 = 16;
/// Tetiğin kartındaki satırlar.
const F_STOP: u32 = 0;
const F_KEY: u32 = 1;
const F_KIND: u32 = 2;
/// Türe göre: düğme, yön ya da gönderilecek tuş.
const F_DETAIL: u32 = 3;
const F_MODE: u32 = 4;
const F_SPEED: u32 = 5;
const F_LIMIT: u32 = 6;
const F_REMOVE: u32 = 7;

fn rid(i: usize, field: u32) -> u32 {
    R_BASE + i as u32 * R_STRIDE + field
}

/// Satır kimliğinden tetik sırası ve satır.
fn split(id: u32) -> Option<(usize, u32)> {
    let d = id.checked_sub(R_BASE)?;
    Some(((d / R_STRIDE) as usize, d % R_STRIDE))
}

/// Hangi tuş atanıyor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bind {
    /// Yeni tetik: basılan tuş yeni tetiğin tuşu olur.
    Add,
    /// Var olan tetiğin tuşu değişiyor.
    Trigger(usize),
    /// Tetiğin klavye eyleminde gönderilecek tuş.
    Send(usize),
}

pub struct Clicker {
    hwnd: HWND,
    settings: Settings,
    /// Süren eylemler; bir tetiğin en çok bir çalıştırması olur.
    runs: Vec<Run>,
    next_run_id: u32,
    /// Her tetik tuşunun kayıt durumu (ayarlardaki sırayla).
    registered: Vec<bool>,
    bind: Option<Bind>,
    visible: bool,
    /// Sürüklenen kaydırıcının satırı.
    drag: Option<u32>,
    scroll: f32,
    hover: Hit,
    pressed: Hit,
    w: f32,
    h: f32,
    head_x: f32,
    head_r: f32,
}

impl Clicker {
    pub fn start(hwnd: HWND) -> Self {
        let mut t = Self {
            hwnd,
            settings: Settings::load(),
            runs: Vec::new(),
            next_run_id: 0,
            registered: Vec::new(),
            bind: None,
            visible: false,
            drag: None,
            scroll: 0.0,
            hover: Hit::None,
            pressed: Hit::None,
            w: 0.0,
            h: 0.0,
            head_x: 0.0,
            head_r: 0.0,
        };
        t.register();
        t
    }

    fn redraw(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn run_pos(&self, trigger: usize) -> Option<usize> {
        self.runs.iter().position(|r| r.trigger == trigger)
    }

    fn running(&self, trigger: usize) -> bool {
        self.run_pos(trigger).is_some()
    }

    // --- Başlat / durdur ---

    fn begin(&mut self, trigger: usize) {
        if self.running(trigger) {
            return;
        }
        let Some(t) = self.settings.triggers.get(trigger) else { return };
        if !t.action.ready() {
            osd::show(ICON_MOUSE, COLOR, t!("Pick the key to send first", "Önce gönderilecek tuşu seç"));
            return;
        }
        self.next_run_id = self.next_run_id.wrapping_add(1);
        self.runs.push(start_run(t, trigger, self.hwnd.0 as usize, 0, self.next_run_id));
        unsafe {
            if self.visible {
                SetTimer(Some(self.hwnd), TIMER_UI, UI_MS, None);
            }
        }
        osd::show(ICON_MOUSE, COLOR, &t.action.label());
        self.redraw();
    }

    /// `runs[pos]`'u bitirir; `tell`: ekranın altında kaç kez yapıldığı görünsün (basılı tutma
    /// bırakılınca gerekmez).
    fn finish(&mut self, pos: usize, tell: bool) {
        let run = self.runs.remove(pos);
        let count = run.count.clone();
        drop(run);
        let n = count.load(Ordering::Relaxed);
        if self.runs.is_empty() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER_UI);
            }
        }
        if tell {
            osd::show(ICON_MOUSE, COLOR, &t!(format!("Stopped · {n}"), format!("Durdu · {n}")));
        }
        self.redraw();
    }

    fn end(&mut self, trigger: usize, tell: bool) {
        if let Some(pos) = self.run_pos(trigger) {
            self.finish(pos, tell);
        }
    }

    /// Hepsini durdurur; `tell` yalnızca sonuncusu için (tek bildirim).
    fn end_all(&mut self, tell: bool) {
        while !self.runs.is_empty() {
            let only = self.runs.len() == 1;
            self.finish(0, tell && only);
        }
    }

    /// Hız, sayı ya da eylem değişti: sürüyorsa yenisiyle devam eder, sayaç kaldığı yerden.
    fn restart(&mut self, trigger: usize) {
        let Some(pos) = self.run_pos(trigger) else { return };
        let Some(t) = self.settings.triggers.get(trigger) else { return };
        if !t.action.ready() {
            self.finish(pos, true);
            return;
        }
        let run = self.runs.remove(pos);
        let count = run.count.clone();
        drop(run);
        let from = count.load(Ordering::Relaxed);
        self.next_run_id = self.next_run_id.wrapping_add(1);
        self.runs.push(start_run(t, trigger, self.hwnd.0 as usize, from, self.next_run_id));
    }

    fn hotkey(&mut self, trigger: usize) {
        let Some(t) = self.settings.triggers.get(trigger) else { return };
        let mode = t.mode;
        if mode == Mode::Hold
            && let Some(pos) = self.run_pos(trigger)
            && self.runs[pos].finished()
        {
            self.finish(pos, false);
        }
        match mode {
            Mode::Hold => self.begin(trigger),
            Mode::Toggle if self.running(trigger) => self.end(trigger, true),
            Mode::Toggle => self.begin(trigger),
        }
    }

    // --- Tetik tuşları ---

    fn unregister(&mut self) {
        for (i, registered) in self.registered.drain(..).enumerate() {
            if registered {
                unsafe {
                    let _ = UnregisterHotKey(Some(self.hwnd), HK_TRIGGER + i as i32);
                }
            }
        }
    }

    fn register(&mut self) {
        self.unregister();
        if !self.settings.enabled {
            return;
        }
        self.registered = self
            .settings
            .triggers
            .iter()
            .enumerate()
            .map(|(i, t)| {
                unsafe {
                    RegisterHotKey(Some(self.hwnd), HK_TRIGGER + i as i32, t.key.mods | MOD_NOREPEAT, t.key.vk as u32)
                }
                .is_ok()
            })
            .collect();
    }

    fn conflict(&self, i: usize) -> bool {
        self.settings.enabled && !self.registered.get(i).copied().unwrap_or(false)
    }

    /// Kapatınca süren eylemler durur, tetik tuşları bırakılır; ayarlar olduğu gibi kalır.
    fn set_enabled(&mut self, on: bool) {
        if self.settings.enabled == on {
            return;
        }
        self.end_all(true);
        self.end_bind(None);
        self.settings.enabled = on;
        self.save();
        self.register();
        self.redraw();
    }

    fn start_bind(&mut self, bind: Bind) {
        self.end_all(true);
        self.unregister();
        self.bind = Some(bind);
        unsafe {
            let _ = SetFocus(Some(self.hwnd));
        }
        self.redraw();
    }

    fn key_used(&self, key: Hotkey, except: Option<usize>) -> bool {
        self.settings.triggers.iter().enumerate().any(|(j, t)| Some(j) != except && t.key == key)
    }

    /// `Some(None)`: tuşu sil; `None`: vazgeç.
    fn end_bind(&mut self, key: Option<Option<Hotkey>>) {
        let Some(bind) = self.bind.take() else { return };
        if let Some(key) = key {
            let used = t!("That key is already a trigger", "Bu tuş zaten bir tetik");
            match (bind, key) {
                (Bind::Add, Some(k)) => {
                    if self.key_used(k, None) {
                        osd::show(ICON_MOUSE, COLOR, used);
                    } else if self.settings.triggers.len() < MAX_TRIGGERS {
                        self.settings.triggers.push(Trigger::new(k));
                        self.scroll = f32::MAX;
                    }
                }
                (Bind::Add, None) => {}
                (Bind::Trigger(i), Some(k)) => {
                    if self.key_used(k, Some(i)) {
                        osd::show(ICON_MOUSE, COLOR, used);
                    } else if let Some(t) = self.settings.triggers.get_mut(i) {
                        t.key = k;
                    }
                }
                (Bind::Trigger(i), None) => self.remove(i),
                (Bind::Send(i), k) => {
                    if let Some(t) = self.settings.triggers.get_mut(i) {
                        t.action = Action::Key(k);
                    }
                }
            }
            self.save();
        }
        self.scroll = self.scroll.min(self.max_scroll());
        self.register();
        self.redraw();
    }

    pub fn binding(&self) -> bool {
        self.bind.is_some()
    }

    pub fn kill_focus(&mut self) {
        self.end_bind(None);
    }

    fn save(&self) {
        if let Err(e) = self.settings.save() {
            ortak::log!("clicker ayarları yazılamadı: {e}");
        }
    }

    // --- Tetik ayarları ---

    fn remove(&mut self, i: usize) {
        if i >= self.settings.triggers.len() {
            return;
        }
        self.end(i, true);
        self.settings.triggers.remove(i);
        for r in &mut self.runs {
            if r.trigger > i {
                r.trigger -= 1;
            }
        }
        self.save();
        self.scroll = self.scroll.min(self.max_scroll());
        self.register();
        self.redraw();
    }

    fn set_action(&mut self, i: usize, a: Action) {
        let Some(t) = self.settings.triggers.get_mut(i) else { return };
        if t.action != a {
            t.action = a;
            self.restart(i);
            self.save();
            self.redraw();
        }
    }

    fn set_mode(&mut self, i: usize, m: Mode) {
        let Some(t) = self.settings.triggers.get_mut(i) else { return };
        if t.mode != m {
            t.mode = m;
            self.end(i, true);
            self.save();
            self.redraw();
        }
    }

    fn set_cps(&mut self, i: usize, cps: f32) {
        let cps = quant_cps(cps);
        let Some(t) = self.settings.triggers.get_mut(i) else { return };
        if t.cps != cps {
            t.cps = cps;
            self.restart(i);
            self.redraw();
        }
    }

    fn set_limit(&mut self, i: usize, n: u32) {
        let n = n.min(LIMIT_MAX as u32);
        let Some(t) = self.settings.triggers.get_mut(i) else { return };
        if t.limit != n {
            t.limit = n;
            self.restart(i);
            self.redraw();
        }
    }

    // --- Yerleşim ---

    fn press_text() -> String {
        t!("press a key", "tuşa bas").into()
    }

    /// Sürerken durum: kaç kez (sınır varsa ondan kaçı).
    fn run_text(&self, i: usize) -> String {
        let n = self.run_pos(i).map_or(0, |p| self.runs[p].count());
        let label = self.settings.triggers[i].action.label();
        match self.settings.triggers[i].limit {
            0 => format!("{label} · {n}"),
            l => format!("{label} · {n} / {l}"),
        }
    }

    /// Bir tetiğin kartı: tuşu, eylemi ve ayrıntısı, modu, hızı, sayısı, kaldırma.
    fn trigger_items(&self, i: usize, t: &Trigger, items: &mut Vec<Item>) {
        let seg = |labels: &[&str], sel: usize| Ctl::Seg(labels.iter().map(|l| l.to_string()).collect(), sel);
        items.push(Item::Section(format!("{}  →  {}", t.key, t.action.label())));
        if self.running(i) {
            items.push(Item::Row(Row::new(
                rid(i, F_STOP),
                self.run_text(i),
                "",
                Ctl::Primary(t!("Stop", "Durdur").into()),
            )));
        }
        let key_sub = if self.conflict(i) {
            t!("Taken by another app: click to change", "Başka uygulamada: değiştirmek için tıkla")
        } else {
            t!("Click to change · Delete removes it", "Değiştirmek için tıkla · Delete kaldırır")
        };
        let key_text = if self.bind == Some(Bind::Trigger(i)) { Self::press_text() } else { t.key.to_string() };
        items.push(Item::Row(Row::new(
            rid(i, F_KEY),
            t!("Trigger key", "Tetik tuşu"),
            key_sub,
            Ctl::Chip(key_text, self.bind == Some(Bind::Trigger(i))),
        )));
        let kind_sub = match t.action.kind() {
            Kind::Click => t!("Clicks where the cursor is", "İmlecin olduğu yere tıklar"),
            Kind::Wheel => t!("Scrolls under the cursor", "İmlecin altında kaydırır"),
            Kind::Key => t!("Presses a keyboard key", "Bir klavye tuşuna basar"),
        };
        let kind_sel = Kind::ALL.iter().position(|&k| k == t.action.kind()).unwrap_or(0);
        items.push(Item::Row(Row::new(
            rid(i, F_KIND),
            t!("Action", "Eylem"),
            kind_sub,
            seg(&Kind::ALL.map(Kind::label), kind_sel),
        )));
        match t.action {
            Action::Click(b) => {
                let sel = Button::ALL.iter().position(|&x| x == b).unwrap_or(0);
                items.push(Item::Row(Row::new(
                    rid(i, F_DETAIL),
                    t!("Button", "Düğme"),
                    "",
                    seg(&Button::ALL.map(Button::label), sel),
                )));
            }
            Action::Wheel(up) => {
                let labels = [t!("Up", "Yukarı"), t!("Down", "Aşağı")];
                items.push(Item::Row(Row::new(
                    rid(i, F_DETAIL),
                    t!("Direction", "Yön"),
                    "",
                    seg(&labels, if up { 0 } else { 1 }),
                )));
            }
            Action::Key(k) => {
                let active = self.bind == Some(Bind::Send(i));
                let text = if active {
                    Self::press_text()
                } else {
                    k.map(|k| k.to_string()).unwrap_or_else(|| t!("pick a key", "tuş seç").into())
                };
                items.push(Item::Row(Row::new(
                    rid(i, F_DETAIL),
                    t!("Key to press", "Basılacak tuş"),
                    t!("Ctrl / Alt / Shift combinations also work", "Ctrl / Alt / Shift birleşimleri de olur"),
                    Ctl::Chip(text, active),
                )));
            }
        }
        let mode_sub = match t.mode {
            Mode::Hold => t!("Repeats while the key is held down", "Tuş basılı olduğu sürece yineler"),
            Mode::Toggle => t!("Press once to start, again to stop", "Bir basış başlatır, bir basış durdurur"),
        };
        let mode_sel = Mode::ALL.iter().position(|&m| m == t.mode).unwrap_or(0);
        items.push(Item::Row(Row::new(
            rid(i, F_MODE),
            t!("Mode", "Mod"),
            mode_sub,
            seg(&Mode::ALL.map(Mode::label), mode_sel),
        )));
        items.push(Item::Row(Row::new(
            rid(i, F_SPEED),
            t!("Speed", "Hız"),
            "",
            Ctl::Slider(cps_to_v(t.cps), fmt_cps(t.cps)),
        )));
        items.push(Item::Row(Row::new(
            rid(i, F_LIMIT),
            t!("Stop after", "Şu kadar sonra dur"),
            "",
            Ctl::Slider(limit_to_v(t.limit), fmt_limit(t.limit)),
        )));
        items.push(Item::Row(Row::new(
            rid(i, F_REMOVE),
            t!("Remove", "Kaldır"),
            t!("Takes this key off the list", "Bu tuşu listeden çıkarır"),
            Ctl::Button(t!("Remove", "Kaldır").into()),
        )));
    }

    /// Sayfanın içeriği: açık / kapalı, her tetiğin kartı, sonda yeni tuş.
    fn items(&self) -> Vec<Item> {
        let s = &self.settings;
        let mut items = vec![
            Item::Section(String::new()),
            Item::Row(Row::new(
                R_ON,
                t!("On", "Açık"),
                t!(
                    "Off releases the trigger keys; your settings stay",
                    "Kapalıyken tetik tuşları bırakılır; ayarların durur"
                ),
                Ctl::Toggle(s.enabled),
            )),
        ];
        if s.triggers.is_empty() {
            items.push(Item::Note(t!("No keys yet: add one below", "Henüz tuş yok: aşağıdan ekle").into(), false));
        }
        for (i, t) in s.triggers.iter().enumerate() {
            self.trigger_items(i, t, &mut items);
        }
        if s.triggers.len() < MAX_TRIGGERS {
            let active = self.bind == Some(Bind::Add);
            let text = if active { Self::press_text() } else { t!("Add key", "Tuş ekle").into() };
            items.push(Item::Section(String::new()));
            items.push(Item::Row(Row::new(
                R_ADD,
                t!("New key", "Yeni tuş"),
                t!(
                    "Each key gets its own action, mode, speed and count",
                    "Her tuşun kendi eylemi, modu, hızı ve sayısı olur"
                ),
                Ctl::Chip(text, active),
            )));
        }
        items
    }

    fn form(&self) -> Form {
        Form::new(self.items(), PAD, self.w - PAD, HEADER + 16.0 - self.scroll.min(1e6))
    }

    fn max_scroll(&self) -> f32 {
        let scroll = self.scroll.min(1e6);
        (self.form().bottom() + scroll - self.h).max(0.0)
    }

    /// Başlıkta düğme yok; altı sayfa kitinin.
    fn hit(&self, g: &Gfx, x: f32, y: f32) -> Hit {
        if y < HEADER {
            return Hit::None;
        }
        self.form().hit(g, x, y)
    }

    /// Sürüklenen kaydırıcıya imlecin x'ini uygular.
    fn drag_to(&mut self, x: f32) {
        let Some(id) = self.drag else { return };
        let Some(v) = self.form().slider_value(id, x) else { return };
        self.slide(id, v);
    }

    fn slide(&mut self, id: u32, v: f32) {
        match split(id) {
            Some((i, F_SPEED)) => self.set_cps(i, v_to_cps(v)),
            Some((i, F_LIMIT)) => self.set_limit(i, v_to_limit(v)),
            _ => {}
        }
    }

    /// Tıklanan (fare bırakılınca).
    fn act(&mut self, hit: Hit) {
        match hit {
            Hit::Toggle(R_ON) | Hit::Row(R_ON) => return self.set_enabled(!self.settings.enabled),
            Hit::Chip(R_ADD) => return self.start_bind(Bind::Add),
            _ => {}
        }
        let Some((i, field)) = hit.id().and_then(split) else { return };
        let Some(t) = self.settings.triggers.get(i) else { return };
        let action = t.action;
        match (field, hit) {
            (F_STOP, Hit::Button(_)) => self.end(i, true),
            (F_KEY, Hit::Chip(_)) => self.start_bind(Bind::Trigger(i)),
            (F_KIND, Hit::Seg(_, k)) => {
                if let Some(&kind) = Kind::ALL.get(k) {
                    self.set_action(i, action.with_kind(kind));
                }
            }
            (F_DETAIL, Hit::Seg(_, k)) => match action {
                Action::Click(_) => {
                    if let Some(&b) = Button::ALL.get(k) {
                        self.set_action(i, Action::Click(b));
                    }
                }
                Action::Wheel(_) if k < 2 => self.set_action(i, Action::Wheel(k == 0)),
                _ => {}
            },
            (F_DETAIL, Hit::Chip(_)) => self.start_bind(Bind::Send(i)),
            (F_MODE, Hit::Seg(_, k)) => {
                if let Some(&m) = Mode::ALL.get(k) {
                    self.set_mode(i, m);
                }
            }
            (F_REMOVE, Hit::Button(_)) => self.remove(i),
            _ => {}
        }
    }

    // --- Çizim ---

    pub fn paint(&self, g: &Gfx) {
        let form = self.form();
        g.clip(Rect::new(0.0, HEADER, self.w, self.h), || form.paint(g, self.hover, self.drag));
    }
}

impl ToolPage for Clicker {
    fn summary(&self) -> Option<(String, String)> {
        let s = &self.settings;
        if !s.enabled {
            return Some((t!("Off", "Kapalı").into(), t!("The keys do nothing", "Tuşlar bir şey yapmaz").into()));
        }
        if s.triggers.is_empty() {
            return Some((t!("No key", "Tuş yok").into(), t!("Add one on the page", "Sayfadan ekle").into()));
        }
        let keys = s.triggers.iter().map(|t| t.key.to_string()).collect::<Vec<_>>().join(", ");
        let what = match s.triggers.as_slice() {
            [t] => format!("{} · {} · {}", t.action.label(), t.mode.label(), fmt_cps(t.cps)),
            all => all.iter().map(|t| t.action.label()).collect::<Vec<_>>().join(" · "),
        };
        Some((keys, what))
    }

    fn message(&mut self, _g: &mut Gfx, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_TIMER if wp.0 == TIMER_UI => self.redraw(),
            WM_HOTKEY if (HK_TRIGGER..HK_TRIGGER + self.registered.len() as i32).contains(&(wp.0 as i32)) => {
                let i = (wp.0 as i32 - HK_TRIGGER) as usize;
                let vk = (lp.0 >> 16) as u16;
                if self.registered[i] && self.settings.triggers.get(i).is_some_and(|t| t.key.vk == vk) {
                    self.hotkey(i);
                }
            }
            WM_DONE => {
                if let Some(pos) = self.runs.iter().position(|r| r.id == wp.0 as u32) {
                    match lp.0 {
                        2 => {
                            self.finish(pos, false);
                            osd::show(ICON_MOUSE, COLOR, t!("Could not send the input", "Girdi gönderilemedi"));
                        }
                        reason => self.finish(pos, reason == 1),
                    }
                }
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn binding(&self) -> bool {
        Clicker::binding(self)
    }

    fn kill_focus(&mut self) {
        Clicker::kill_focus(self)
    }

    fn layout(&mut self, w: f32, h: f32, head_x: f32, head_r: f32) {
        self.w = w;
        self.h = h;
        self.head_x = head_x;
        self.head_r = head_r;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn paint(&self, g: &Gfx) {
        Clicker::paint(self, g)
    }

    fn mouse_move(&mut self, g: &Gfx, x: f32, y: f32) {
        if self.drag.is_some() {
            self.drag_to(x);
            return;
        }
        let hit = self.hit(g, x, y);
        if hit != self.hover {
            self.hover = hit;
            self.redraw();
        }
    }

    fn mouse_leave(&mut self) {
        if self.hover != Hit::None {
            self.hover = Hit::None;
            self.redraw();
        }
    }

    fn mouse_down(&mut self, g: &Gfx, x: f32, y: f32) {
        self.pressed = self.hit(g, x, y);
        if let Hit::Slider(id, v) = self.pressed {
            self.end_bind(None);
            self.drag = Some(id);
            self.slide(id, v);
        }
    }

    fn mouse_up(&mut self, g: &Gfx, x: f32, y: f32) {
        if self.drag.take().is_some() {
            self.save();
            self.pressed = Hit::None;
            self.hover = self.hit(g, x, y);
            self.redraw();
            return;
        }
        let hit = self.hit(g, x, y);
        if !hit.same(std::mem::replace(&mut self.pressed, Hit::None)) {
            return;
        }
        self.end_bind(None);
        self.act(hit);
    }

    fn wheel(&mut self, g: &Gfx, x: f32, y: f32, delta: f32) {
        if let Hit::Slider(id, _) = self.hit(g, x, y)
            && let Some((i, field)) = split(id)
            && let Some(t) = self.settings.triggers.get(i)
        {
            match field {
                F_SPEED => {
                    let c = t.cps;
                    self.set_cps(i, c + delta.signum() * cps_step(if delta > 0.0 { c } else { c - 0.01 }));
                    self.save();
                    return;
                }
                F_LIMIT => {
                    let n = t.limit;
                    let step = limit_step(if delta > 0.0 { n } else { n.saturating_sub(1) });
                    let next = if delta > 0.0 { n.saturating_add(step) } else { n.saturating_sub(step) };
                    self.set_limit(i, next);
                    self.save();
                    return;
                }
                _ => {}
            }
        }
        self.scroll = (self.scroll - delta * 60.0).clamp(0.0, self.max_scroll());
        self.hover = self.hit(g, x, y);
        self.redraw();
    }

    /// Tuş atanıyorsa yakalar: Esc vazgeçer, Backspace / Delete siler.
    fn key(&mut self, vk: u16) -> bool {
        if self.bind.is_none() {
            return false;
        }
        if !hotkey::is_modifier(vk) {
            let key = match VIRTUAL_KEY(vk) {
                VK_ESCAPE => None,
                VK_BACK | VK_DELETE => Some(None),
                _ => Some(Some(Hotkey::pressed(vk))),
            };
            self.end_bind(key);
        }
        true
    }

    fn interactive(&self, g: &Gfx, x: f32, y: f32) -> bool {
        self.hit(g, x, y) != Hit::None
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        unsafe {
            if visible && !self.runs.is_empty() {
                SetTimer(Some(self.hwnd), TIMER_UI, UI_MS, None);
            } else {
                let _ = KillTimer(Some(self.hwnd), TIMER_UI);
            }
        }
        if !visible {
            self.hover = Hit::None;
            self.drag = None;
            self.end_bind(None);
        }
    }
}

impl Drop for Clicker {
    fn drop(&mut self) {
        self.runs.clear();
        self.unregister();
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_UI);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Hotkey {
        Hotkey::parse(s).unwrap()
    }

    #[test]
    fn fresh_settings_have_f6_left_click() {
        let s = Settings::parse("");
        assert_eq!(s.triggers.len(), 1);
        assert_eq!(s.triggers[0].key, key("F6"));
        assert_eq!(s.triggers[0].action, Action::Click(Button::Left));
        assert_eq!(s.triggers[0].mode, Mode::Hold);
    }

    #[test]
    fn old_single_key_settings_become_one_trigger() {
        let s = Settings::parse("acik=1\ntus=F7\nmod=ackapa\ndugme=orta\nhiz=25\nsayi=100\n");
        assert_eq!(s.triggers.len(), 1);
        let t = &s.triggers[0];
        assert_eq!(t.key, key("F7"));
        assert_eq!(t.action, Action::Click(Button::Middle));
        assert_eq!(t.mode, Mode::Toggle);
        assert_eq!(t.cps, 25.0);
        assert_eq!(t.limit, 100);
    }

    #[test]
    fn old_key_list_becomes_triggers_with_shared_settings() {
        let s = Settings::parse("tus=F6\ntuslar=F7,F8,F7,F9\ndugme=sag\n");
        let keys: Vec<Hotkey> = s.triggers.iter().map(|t| t.key).collect();
        assert_eq!(keys, [key("F7"), key("F8"), key("F9")]);
        assert!(s.triggers.iter().all(|t| t.action == Action::Click(Button::Right)));
    }

    #[test]
    fn old_empty_key_list_disables_trigger() {
        assert!(Settings::parse("tuslar=\n").triggers.is_empty());
    }

    #[test]
    fn new_format_round_trips_every_action() {
        let mut s = Settings { enabled: false, triggers: Vec::new() };
        s.triggers.push(Trigger {
            key: key("F6"),
            action: Action::Click(Button::Right),
            mode: Mode::Toggle,
            cps: 2.5,
            limit: 50,
        });
        s.triggers.push(Trigger {
            key: key("Ctrl+F7"),
            action: Action::Wheel(false),
            mode: Mode::Hold,
            cps: 10.0,
            limit: 0,
        });
        s.triggers.push(Trigger {
            key: key("F8"),
            action: Action::Key(Some(key("Ctrl+Space"))),
            mode: Mode::Hold,
            cps: 100.0,
            limit: 0,
        });
        s.triggers.push(Trigger { key: key("F9"), action: Action::Key(None), mode: Mode::Hold, cps: 1.0, limit: 1 });
        let text = {
            let mut text = format!("acik={}\nsurum=2\n", s.enabled as u8);
            for t in &s.triggers {
                text.push_str(&format!("tetik={}\n", t.line()));
            }
            text
        };
        let back = Settings::parse(&text);
        assert!(!back.enabled);
        assert_eq!(back.triggers, s.triggers);
    }

    #[test]
    fn new_format_with_no_triggers_stays_empty() {
        assert!(Settings::parse("acik=1\nsurum=2\n").triggers.is_empty());
    }

    #[test]
    fn duplicate_trigger_keys_are_dropped() {
        let s = Settings::parse("surum=2\ntetik=F6|sol|basili|10|0\ntetik=F6|sag|basili|10|0\n");
        assert_eq!(s.triggers.len(), 1);
        assert_eq!(s.triggers[0].action, Action::Click(Button::Left));
    }

    #[test]
    fn row_ids_split_back() {
        for i in [0, 1, 7, 31] {
            for f in [F_STOP, F_KEY, F_KIND, F_DETAIL, F_MODE, F_SPEED, F_LIMIT, F_REMOVE] {
                assert_eq!(split(rid(i, f)), Some((i, f)));
            }
        }
        assert_eq!(split(R_ON), None);
        assert_eq!(split(R_ADD), None);
    }

    #[test]
    fn changing_kind_keeps_same_kind_detail() {
        let a = Action::Click(Button::Middle);
        assert_eq!(a.with_kind(Kind::Click), a);
        assert_eq!(a.with_kind(Kind::Wheel), Action::Wheel(true));
        assert_eq!(a.with_kind(Kind::Key), Action::Key(None));
        assert!(!Action::Key(None).ready());
    }
}

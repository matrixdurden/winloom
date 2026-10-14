//! lights: Bluetooth LED ışıklar. Renk, parlaklık, ışığın kendi efektleri ve müzikle yanma.
//! Desteklenen ürünler `surucu/` altında (şimdilik STARLIGHT); sayfa ve müzik modeli bilmez,
//! `surucu::Cmd` gönderir.
//!
//! Üç sekme: Işık (cihaz, aç/kapa, parlaklık, renk), Efektler (arama, hız, yön, liste), Müzik
//! (vuruşta renk değiştir ya da parla; canlı ölçer). Bluetooth işi `link`'in iş parçacığında,
//! ses yakalama `music`'inkinde; ikisi de arayüzü bekletmez.

#[macro_use]
extern crate ortak;

mod link;
mod listen;
mod music;
pub mod surucu;
mod tool;
pub use tool::TOOL;

use std::path::PathBuf;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use ortak::form::{self, Ctl, Form, Item, Row, TABS_H};
use ortak::gfx::{Color, Gfx, Rect};
use ortak::ui::*;
use ortak::util;

use link::{Link, State, WM_LINK};
use music::{Listen, Music, React, Source};
use surucu::{Cmd, Model};

/// Müzik sekmesindeki ölçer tazelenir.
pub const TIMER_METER: usize = 901;
/// Başka bir araçtan (asistan) istek geldi: `hub::serve("lights")`.
const WM_ASK: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 91;
const METER_MS: u32 = 33;

/// Hazır renkler: (renk, İngilizce, Türkçe).
const PRESETS: [(u32, &str, &str); 10] = [
    (0xFF2020, "Red", "Kırmızı"),
    (0xFF7A00, "Orange", "Turuncu"),
    (0xFFD000, "Yellow", "Sarı"),
    (0x20E040, "Green", "Yeşil"),
    (0x00D8E0, "Cyan", "Turkuaz"),
    (0x1060FF, "Blue", "Mavi"),
    (0x8A2BFF, "Purple", "Mor"),
    (0xFF1A8C, "Pink", "Pembe"),
    (0xFFB070, "Warm white", "Sıcak beyaz"),
    (0xFFFFFF, "White", "Beyaz"),
];

pub fn dir() -> PathBuf {
    util::data_dir().join("lights")
}

fn ini() -> PathBuf {
    dir().join("ayarlar.ini")
}

pub fn installed() -> bool {
    dir().is_dir()
}

#[derive(Clone)]
struct Settings {
    /// Son bağlanılan ışığın Bluetooth adresi (0: yok) ve modeli.
    address: u64,
    name: String,
    model: String,
    /// winloom açılınca son ışığa bağlan.
    auto: bool,
    power: bool,
    color: [u8; 3],
    /// 0..1
    brightness: f32,
    /// Çalan efekt; 0: düz renk.
    effect: u16,
    speed: f32,
    reverse: bool,
    music: bool,
    listen: Listen,
    react: React,
    source: Source,
    sensitivity: f32,
    /// Işığın zamanlaması (ms): artı geciktirir (Bluetooth kulaklıkta ses geç gelir), eksi önden.
    delay: i32,
    /// Müzikte bas ve trampet vuruşunun rengi.
    bass: [u8; 3],
    snare: [u8; 3],
}

impl Settings {
    fn load() -> Self {
        let mut s = Settings {
            address: 0,
            name: String::new(),
            model: String::new(),
            auto: true,
            power: true,
            color: [255, 255, 255],
            brightness: 1.0,
            effect: 0,
            speed: 0.5,
            reverse: false,
            music: false,
            listen: Listen::All,
            react: React::Colors,
            source: Source::Drums,
            sensitivity: 1.5,
            delay: 0,
            bass: rgb(PRESETS[0].0),
            snare: rgb(PRESETS[5].0),
        };
        let text = std::fs::read_to_string(ini()).unwrap_or_default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "adres" => s.address = u64::from_str_radix(v, 16).unwrap_or(0),
                "ad" => s.name = v.to_string(),
                "model" => s.model = v.to_string(),
                "oto" => s.auto = v != "0",
                "acik" => s.power = v != "0",
                "renk" => s.color = u32::from_str_radix(v, 16).map_or(s.color, rgb),
                "parlaklik" => s.brightness = v.parse().map_or(s.brightness, |b: f32| b.clamp(0.0, 1.0)),
                "efekt" => s.effect = v.parse().unwrap_or(0),
                "hiz" => s.speed = v.parse().map_or(s.speed, |b: f32| b.clamp(0.0, 1.0)),
                "ters" => s.reverse = v == "1",
                "muzik" => s.music = v == "1",
                "dinle" => s.listen = if v == "muzik" { Listen::Music } else { Listen::All },
                "tepki" => s.react = if v == "flas" { React::Flash } else { React::Colors },
                "kaynak" => s.source = if v == "ritim" { Source::Beat } else { Source::Drums },
                "gecikme" => s.delay = v.parse().map_or(0, |d: i32| d.clamp(-DELAY_EARLY, DELAY_MAX)),
                // Eski sürümün hazır ikilileri (kırmızı · mavi, mor · turkuaz, pembe · sarı, yeşil · mor).
                "cift" => {
                    if let Some(&[a, b]) = v.parse::<usize>().ok().and_then(|i| [[0, 5], [6, 4], [7, 2], [3, 6]].get(i)) {
                        (s.bass, s.snare) = (rgb(PRESETS[a].0), rgb(PRESETS[b].0));
                    }
                }
                "bas_renk" => s.bass = u32::from_str_radix(v, 16).map_or(s.bass, rgb),
                "trampet_renk" => s.snare = u32::from_str_radix(v, 16).map_or(s.snare, rgb),
                "hassasiyet" => s.sensitivity = v.parse().map_or(s.sensitivity, |b: f32| b.clamp(SENS_MIN, SENS_MAX)),
                _ => {}
            }
        }
        s
    }

    fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(dir())?;
        let [r, g, b] = self.color;
        let text = format!(
            "adres={:X}\nad={}\nmodel={}\noto={}\nacik={}\nrenk={r:02X}{g:02X}{b:02X}\nparlaklik={}\nefekt={}\nhiz={}\nters={}\nmuzik={}\ndinle={}\ntepki={}\nkaynak={}\nhassasiyet={}\ngecikme={}\nbas_renk={:06X}\ntrampet_renk={:06X}\n",
            self.address,
            self.name,
            self.model,
            self.auto as u8,
            self.power as u8,
            self.brightness,
            self.effect,
            self.speed,
            self.reverse as u8,
            self.music as u8,
            if self.listen == Listen::Music { "muzik" } else { "hepsi" },
            if self.react == React::Flash { "flas" } else { "renk" },
            if self.source == Source::Beat { "ritim" } else { "davul" },
            self.sensitivity,
            self.delay,
            hex(self.bass),
            hex(self.snare),
        );
        std::fs::write(ini(), text)
    }

    fn music_config(&self) -> music::Config {
        music::Config {
            listen: self.listen,
            react: self.react,
            source: self.source,
            sensitivity: self.sensitivity,
            brightness: self.brightness.max(0.05),
            delay: self.delay,
            colors: [self.bass, self.snare],
        }
    }
}

fn rgb(c: u32) -> [u8; 3] {
    [(c >> 16) as u8, (c >> 8) as u8, c as u8]
}

fn hex(c: [u8; 3]) -> u32 {
    (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
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

fn cli(args: &[String]) -> Option<i32> {
    match args.first()?.as_str() {
        "--lights-test" | "--isiklar-dene" => {
            ortak::tool::console(false);
            Some(link::cli_test())
        }
        "--lights-script" | "--isiklar-betik" => {
            ortak::tool::console(false);
            let address = args.get(3).and_then(|a| u64::from_str_radix(a, 16).ok());
            Some(link::cli_script(args.get(1)?, args.get(2)?, address))
        }
        "--lights-pace" | "--isiklar-tempo" => {
            ortak::tool::console(false);
            Some(link::cli_pace(args.iter().any(|a| a == "cevapli" || a == "--with-response")))
        }
        _ => None,
    }
}

// --- Kaydırıcılar ---

/// Hassasiyet: eşik çarpanı. Kaydırıcının sağı daha hassas (küçük çarpan).
const SENS_MIN: f32 = 0.5;
const SENS_MAX: f32 = 3.0;

fn sens_to_v(s: f32) -> f32 {
    ((SENS_MAX - s) / (SENS_MAX - SENS_MIN)).clamp(0.0, 1.0)
}

fn v_to_sens(v: f32) -> f32 {
    ((SENS_MAX - v.clamp(0.0, 1.0) * (SENS_MAX - SENS_MIN)) * 10.0).round() / 10.0
}

fn pct(v: f32) -> String {
    format!("%{}", (v * 100.0).round() as u32)
}

// --- Sayfa ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Light,
    Effects,
    Music,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Light, Tab::Effects, Tab::Music];
}

fn tab_labels() -> [&'static str; 3] {
    [t!("Light", "Işık"), t!("Effects", "Efektler"), t!("Music", "Müzik")]
}

/// Satır ve alan kimlikleri.
const R_DEVICE: u32 = 1;
const R_AUTO: u32 = 2;
const R_POWER: u32 = 3;
const R_BRIGHT: u32 = 4;
const C_COLORS: u32 = 5;
const C_SEARCH: u32 = 6;
const R_SPEED: u32 = 7;
const R_DIR: u32 = 8;
const R_MUSIC: u32 = 9;
const R_REACT: u32 = 10;
const R_SOURCE: u32 = 11;
const R_SENS: u32 = 12;
const C_METER: u32 = 13;
const R_DELAY: u32 = 14;
/// Müzikte bas ve trampet rengi seçicileri.
const C_BASS: u32 = 15;
const C_SNARE: u32 = 17;
const R_LISTEN: u32 = 16;

/// Işık zamanlamasının sınırları (ms): bu kadar önden, bu kadar geriden.
const DELAY_EARLY: i32 = 300;
const DELAY_MAX: i32 = 400;

fn delay_to_v(d: i32) -> f32 {
    (d + DELAY_EARLY) as f32 / (DELAY_EARLY + DELAY_MAX) as f32
}

/// 10 ms adımlarla.
fn v_to_delay(v: f32) -> i32 {
    ((v.clamp(0.0, 1.0) * (DELAY_EARLY + DELAY_MAX) as f32 / 10.0).round() * 10.0) as i32 - DELAY_EARLY
}
/// Efekt satırları: bu + efektin kimliği.
const R_EFFECT: u32 = 1000;

/// Renk alanı: hazır renkler ve altında ton şeridi.
const COLORS_H: f32 = 112.0;
const SWATCH: f32 = 30.0;
const SEARCH_H: f32 = 36.0;
const METER_H: f32 = 108.0;
/// Müzikteki renk seçicisi: solda adı, sağda hazır renkler.
const PICK_H: f32 = 62.0;
const PICK_LABEL: f32 = 110.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    None,
    Tab(Tab),
    Form(form::Hit),
    Swatch(usize),
    /// Müzikteki renk seçicisi (`C_BASS` / `C_SNARE`) ve hazır rengin sırası.
    Pick(u32, usize),
    /// Ton şeridi: 0..1.
    Hue(f32),
    Search,
}

impl Hit {
    fn form(self) -> form::Hit {
        if let Hit::Form(h) = self { h } else { form::Hit::None }
    }

    fn same(self, other: Hit) -> bool {
        match (self, other) {
            (Hit::Form(a), Hit::Form(b)) => a.same(b),
            (Hit::Hue(_), Hit::Hue(_)) => true,
            (a, b) => a == b,
        }
    }
}

pub struct Lights {
    hwnd: HWND,
    settings: Settings,
    link: Link,
    music: Option<Music>,
    tab: Tab,
    search: TextField,
    scroll: [f32; 3],
    hover: Hit,
    pressed: Hit,
    /// Sürüklenen kaydırıcının satırı (ton şeridi: `C_COLORS`).
    drag: Option<u32>,
    /// Başka araçlardan gelen istekler (`answer`).
    asks: std::sync::mpsc::Receiver<ortak::hub::Request>,
    visible: bool,
    w: f32,
    h: f32,
}

impl Lights {
    pub fn start(hwnd: HWND) -> Self {
        let settings = Settings::load();
        let link = Link::start(hwnd);
        let mut t = Self {
            hwnd,
            settings,
            link,
            music: None,
            tab: Tab::Light,
            search: TextField::new(40),
            scroll: [0.0; 3],
            hover: Hit::None,
            pressed: Hit::None,
            drag: None,
            asks: ortak::hub::serve("lights", hwnd, WM_ASK),
            visible: false,
            w: 0.0,
            h: 0.0,
        };
        if t.settings.auto && t.settings.address != 0 {
            t.connect();
        }
        if t.settings.music {
            t.music = Some(Music::start(t.link.tx(), t.settings.music_config()));
        }
        t
    }

    fn redraw(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn save(&self) {
        if let Err(e) = self.settings.save() {
            ortak::log!("lights ayarları yazılamadı: {e}");
        }
    }

    /// Bağlı ya da son bağlanılan ışığın modeli; hiç yoksa ilk desteklenen.
    fn model(&self) -> &'static Model {
        self.link.status().model.or_else(|| surucu::by_id(&self.settings.model)).unwrap_or(surucu::MODELS[0])
    }

    fn connect(&self) {
        self.link.connect((self.settings.address != 0).then_some(self.settings.address));
    }

    /// Bağlantı durumu değişti: yeni ışığa bağlandıysa hatırlanır.
    fn on_link(&mut self) {
        let s = self.link.status();
        if s.state == State::Connected && (s.address != self.settings.address || s.name != self.settings.name) {
            self.settings.address = s.address;
            self.settings.name = s.name.clone();
            self.settings.model = s.model.map(|m| m.id.to_string()).unwrap_or_default();
            self.save();
        }
        self.redraw();
    }

    // --- Işığa gidenler ---

    fn set_power(&mut self, on: bool) {
        self.settings.power = on;
        self.link.send(Cmd::Power(on));
        self.save();
        self.redraw();
    }

    fn set_color(&mut self, c: [u8; 3]) {
        self.settings.color = c;
        self.settings.effect = 0;
        if !self.settings.power {
            self.settings.power = true;
            self.link.send(Cmd::Power(true));
        }
        self.link.send(Cmd::Color(c));
        self.redraw();
    }

    fn set_brightness(&mut self, v: f32) {
        let v = (v * 100.0).round() / 100.0;
        if self.settings.brightness != v {
            self.settings.brightness = v;
            self.link.send(Cmd::Brightness(v.max(0.01)));
            self.update_music();
            self.redraw();
        }
    }

    fn set_effect(&mut self, id: u16) {
        self.settings.effect = id;
        if !self.settings.power {
            self.settings.power = true;
            self.link.send(Cmd::Power(true));
        }
        self.link.send(Cmd::Effect(id));
        self.save();
        self.redraw();
    }

    fn set_speed(&mut self, v: f32) {
        let v = (v * 100.0).round() / 100.0;
        if self.settings.speed != v {
            self.settings.speed = v;
            self.link.send(Cmd::Speed(v));
            self.redraw();
        }
    }

    fn set_reverse(&mut self, r: bool) {
        if self.settings.reverse != r {
            self.settings.reverse = r;
            self.link.send(Cmd::Reverse(r));
            self.save();
            self.redraw();
        }
    }

    // --- Müzik ---

    fn set_music(&mut self, on: bool) {
        if self.settings.music == on {
            return;
        }
        self.settings.music = on;
        if on {
            self.music = Some(Music::start(self.link.tx(), self.settings.music_config()));
        } else {
            self.music = None;
            // Müzikten önceki hali geri gelir.
            self.link.send(Cmd::Brightness(self.settings.brightness.max(0.01)));
            if self.settings.effect > 0 {
                self.link.send(Cmd::Effect(self.settings.effect));
            } else {
                self.link.send(Cmd::Color(self.settings.color));
            }
        }
        self.save();
        self.update_timer();
        self.redraw();
    }

    // --- Başka araçlardan istek (asistan) ---

    /// `hub::ask("lights", ...)`: "status", "on", "off", "color <#RRGGBB ya da hazır rengin adı>",
    /// "brightness <0-100>", "music_on", "music_off". Renk istenince müzikle yanma kapanır (yoksa
    /// müzik rengi hemen değiştirir). Cevap ışığın son hali.
    fn answer(&mut self, body: &str) -> Result<String, String> {
        let body = body.trim();
        let (cmd, arg) = body.split_once(' ').unwrap_or((body, ""));
        let arg = arg.trim();
        match cmd {
            "status" => {}
            "on" => self.set_power(true),
            "off" => {
                self.set_music(false);
                self.set_power(false);
            }
            "color" => {
                let c = parse_color(arg).ok_or_else(|| format!("renk anlaşılmadı: {arg}"))?;
                self.set_music(false);
                self.set_color(c);
                self.save();
            }
            "brightness" => {
                let v: f32 = arg.trim_end_matches('%').trim().parse().map_err(|_| format!("parlaklık anlaşılmadı: {arg}"))?;
                if !self.settings.power {
                    self.set_power(true);
                }
                self.set_brightness((v / 100.0).clamp(0.01, 1.0));
                self.save();
            }
            "music_on" => self.set_music(true),
            "music_off" => self.set_music(false),
            _ => return Err(format!("bilinmeyen istek: {cmd}")),
        }
        let st = self.link.status();
        if st.state == State::Off && self.settings.address != 0 {
            self.connect();
        }
        let s = &self.settings;
        let link = match st.state {
            State::Connected => format!("{} bağlı", st.name),
            State::Off if s.address == 0 => "hiç ışık bağlanmadı (Işıklar sayfasından bağlanmalı)".to_string(),
            _ => "ışık şu an bağlı değil, bağlanıyor; komut bağlanınca gider".to_string(),
        };
        let color = PRESETS
            .iter()
            .find(|p| p.0 == hex(s.color))
            .map_or_else(|| format!("#{:06X}", hex(s.color)), |p| format!("{} (#{:06X})", p.2, p.0));
        let onoff = |b: bool| if b { "açık" } else { "kapalı" };
        Ok(format!("{link} · {} · renk {color} · parlaklık {} · müzikle yanma {}", onoff(s.power), pct(s.brightness), onoff(s.music)))
    }

    fn update_music(&self) {
        if let Some(m) = &self.music {
            m.set(self.settings.music_config());
        }
    }

    fn update_timer(&self) {
        unsafe {
            if self.visible && self.tab == Tab::Music && self.music.is_some() {
                SetTimer(Some(self.hwnd), TIMER_METER, METER_MS, None);
            } else {
                let _ = KillTimer(Some(self.hwnd), TIMER_METER);
            }
        }
    }

    fn select(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.search.blur();
            self.update_timer();
            self.redraw();
        }
    }

    // --- Yerleşim ---

    fn device_row(&self) -> Row {
        let s = self.link.status();
        let known = if self.settings.name.is_empty() { self.model().name.to_string() } else { self.settings.name.clone() };
        match s.state {
            State::Off => {
                let sub = s.error.clone().unwrap_or_else(|| {
                    t!("Turn the light on and close its phone app, then connect", "Işığı aç, telefon uygulamasını kapat, sonra bağlan").into()
                });
                let title = if self.settings.address != 0 {
                    t!(format!("{known} · not connected"), format!("{known} · bağlı değil"))
                } else {
                    t!("Not connected", "Bağlı değil").into()
                };
                Row::new(R_DEVICE, title, sub, Ctl::Primary(t!("Connect", "Bağlan").into()))
            }
            State::Searching => Row::new(
                R_DEVICE,
                t!("Searching…", "Aranıyor…"),
                t!("Looking for lights nearby (up to 10 s)", "Yakındaki ışıklara bakılıyor (en fazla 10 sn)"),
                Ctl::None,
            ),
            State::Connecting => {
                let name = if s.name.is_empty() { known } else { s.name.clone() };
                Row::new(R_DEVICE, t!(format!("Connecting · {name}"), format!("Bağlanıyor · {name}")), "", Ctl::None)
            }
            State::Connected => {
                let sub = match (&s.error, s.reachable) {
                    (_, false) => t!("Out of reach · Windows keeps trying", "Ulaşılamıyor · Windows yeniden deniyor").to_string(),
                    (Some(e), _) => e.clone(),
                    (None, true) => t!("Connected", "Bağlı").into(),
                };
                Row::new(R_DEVICE, s.name.clone(), sub, Ctl::Button(t!("Disconnect", "Bağlantıyı kes").into()))
            }
        }
    }

    fn light_items(&self) -> Vec<Item> {
        let s = &self.settings;
        let off = !self.link.connected();
        vec![
            Item::Section(t!("Device", "Cihaz").into()),
            Item::Row(self.device_row()),
            Item::Row(Row::new(
                R_AUTO,
                t!("Connect on start", "Açılışta bağlan"),
                t!("Connects to the last light when winloom starts", "winloom açılınca son ışığa kendiliğinden bağlanır"),
                Ctl::Toggle(s.auto),
            )),
            Item::Section(t!("Light", "Işık").into()),
            Item::Row(Row::new(R_POWER, t!("On", "Açık"), "", Ctl::Toggle(s.power)).dim(off)),
            Item::Row(Row::new(R_BRIGHT, t!("Brightness", "Parlaklık"), "", Ctl::Slider(s.brightness, pct(s.brightness))).dim(off)),
            Item::Section(t!("Color", "Renk").into()),
            Item::Custom(C_COLORS, COLORS_H),
        ]
    }

    /// Aramaya uyan efektler.
    fn shown(&self) -> Vec<&'static surucu::Effect> {
        let q = self.search.text.trim().to_lowercase();
        self.model()
            .effects
            .iter()
            .filter(|e| q.is_empty() || surucu::effect_name(e).to_lowercase().contains(&q) || e.name.to_lowercase().contains(&q) || e.id.to_string() == q)
            .collect()
    }

    fn effect_items(&self) -> Vec<Item> {
        let s = &self.settings;
        let off = !self.link.connected();
        let mut v = vec![Item::Custom(C_SEARCH, SEARCH_H), Item::Section(String::new())];
        v.push(Item::Row(
            Row::new(R_SPEED, t!("Speed", "Hız"), "", Ctl::Slider(s.speed, pct(s.speed))).dim(off),
        ));
        if self.model().reverse {
            v.push(Item::Row(
                Row::new(
                    R_DIR,
                    t!("Direction", "Yön"),
                    "",
                    Ctl::Seg(vec![t!("Forward", "İleri").into(), t!("Reverse", "Geri").into()], s.reverse as usize),
                )
                .dim(off),
            ));
        }
        let shown = self.shown();
        v.push(Item::Section(t!(format!("Effects · {}", shown.len()), format!("Efektler · {}", shown.len()))));
        if shown.is_empty() {
            v.push(Item::Note(t!("No effect matches", "Uyan efekt yok").into(), false));
        }
        for e in shown {
            let on = s.effect == e.id && !s.music;
            v.push(Item::Row(Row::new(R_EFFECT + e.id as u32, format!("{:03}  {}", e.id, surucu::effect_name(e)), "", Ctl::Check(on)).clickable()));
        }
        v
    }

    fn music_items(&self) -> Vec<Item> {
        let s = &self.settings;
        let react_sub = match s.react {
            React::Colors => t!("Two colors swap on every beat", "Her vuruşta iki renk yer değiştirir"),
            React::Flash => t!("Flashes on every beat, dim in between", "Her vuruşta yanıp söner"),
        };
        let source_sub = match s.source {
            Source::Beat => t!("Locks onto the tempo: the light changes right on the beat", "Tempoya kilitlenir, ışık vuruşla aynı anda değişir"),
            Source::Drums => t!("Follows kick and snare hits, a little behind; hi-hats are left out", "Bas ve trampet darbelerini izler, biraz geriden gelir; ziller sayılmaz"),
        };
        let listen_sub = match (s.listen, self.music.as_ref().and_then(|m| m.meter().app)) {
            (Listen::All, _) => t!("Everything the computer plays, Discord and games too", "Bilgisayarda çalan her şey, Discord ve oyunlar da").to_string(),
            (Listen::Music, Some(app)) => t!(format!("Only Spotify or a browser · now {app}"), format!("Yalnızca Spotify ya da tarayıcı · şu an {app}")),
            (Listen::Music, None) => t!("Only Spotify or a browser · nothing playing", "Yalnızca Spotify ya da tarayıcı · çalan yok").to_string(),
        };
        let mut v = vec![
            Item::Section(t!("Music", "Müzik").into()),
            Item::Row(Row::new(
                R_MUSIC,
                t!("Move with the music", "Müzikle yansın"),
                t!("Listens to what your computer plays, no microphone", "Bilgisayarda çalanı dinler, mikrofon gerekmez"),
                Ctl::Toggle(s.music),
            )),
            Item::Row(Row::new(
                R_LISTEN,
                t!("Listen to", "Dinlenen ses"),
                listen_sub,
                Ctl::Seg(vec![t!("All sound", "Tüm ses").into(), t!("Spotify · browser", "Spotify · tarayıcı").into()], (s.listen == Listen::Music) as usize),
            )),
            Item::Row(Row::new(
                R_REACT,
                t!("On a beat", "Vuruşta"),
                react_sub,
                Ctl::Seg(vec![t!("Color", "Renk").into(), t!("Flash", "Flaş").into()], (s.react == React::Flash) as usize),
            )),
            Item::Row(Row::new(
                R_SOURCE,
                t!("Beats", "Vuruşlar"),
                source_sub,
                Ctl::Seg(vec![t!("Rhythm", "Ritim").into(), t!("Drums", "Davul").into()], (s.source == Source::Drums) as usize),
            )),
            Item::Row(Row::new(
                R_SENS,
                t!("Sensitivity", "Hassasiyet"),
                t!("Higher catches quieter beats", "Arttıkça daha hafif vuruşlar da yakalanır"),
                Ctl::Slider(sens_to_v(s.sensitivity), pct(sens_to_v(s.sensitivity))),
            )),
            Item::Row(Row::new(
                R_DELAY,
                t!("Light timing", "Işık zamanlaması"),
                t!("Light behind the music: slide left. Ahead (Bluetooth headphones): slide right", "Işık geriden geliyorsa sola, önden gidiyorsa (Bluetooth kulaklık) sağa çek"),
                Ctl::Slider(delay_to_v(s.delay), if s.delay == 0 { "0 ms".into() } else { format!("{:+} ms", s.delay) }),
            )),
        ];
        // Renk'te bas ve trampet ayrı renk, Flaş'ta tek renk.
        v.push(Item::Section(t!("Colors", "Renkler").into()));
        v.push(Item::Custom(C_BASS, PICK_H));
        if s.react == React::Colors {
            v.push(Item::Custom(C_SNARE, PICK_H));
        }
        if s.music {
            if !self.link.connected() {
                v.push(Item::Note(t!("No light connected: the beats only show here", "Işık bağlı değil: vuruşlar yalnızca burada görünür").into(), false));
            }
            v.push(Item::Section(t!("Now", "Şu an").into()));
            v.push(Item::Custom(C_METER, METER_H));
        }
        v
    }

    fn content_top(&self) -> f32 {
        HEADER + TABS_H + 16.0 - self.scroll[self.tab as usize]
    }

    fn form(&self) -> Form {
        let items = match self.tab {
            Tab::Light => self.light_items(),
            Tab::Effects => self.effect_items(),
            Tab::Music => self.music_items(),
        };
        Form::new(items, PAD, self.w - PAD, self.content_top())
    }

    fn max_scroll(&self) -> f32 {
        (self.form().bottom() + self.scroll[self.tab as usize] - self.h).max(0.0)
    }

    /// Renk alanının parçaları: hazır renklerin yerleri ve ton şeridi.
    fn color_rects(r: Rect) -> (Vec<Rect>, Rect) {
        let inner = Rect::new(r.l + 16.0, r.t + 16.0, r.r - 16.0, r.b - 16.0);
        let gap = ((inner.w() - SWATCH * PRESETS.len() as f32) / (PRESETS.len() - 1) as f32).clamp(6.0, 18.0);
        let swatches = (0..PRESETS.len())
            .map(|i| {
                let x = inner.l + i as f32 * (SWATCH + gap);
                Rect::new(x, inner.t, x + SWATCH, inner.t + SWATCH)
            })
            .collect();
        let hue = Rect::new(inner.l, inner.b - 22.0, inner.r, inner.b);
        (swatches, hue)
    }

    /// Müzikteki renk seçicisinin hazır renklerinin yerleri.
    fn pick_rects(r: Rect) -> Vec<Rect> {
        let inner = Rect::new(r.l + 16.0 + PICK_LABEL, r.t + 16.0, r.r - 16.0, r.b - 16.0);
        let gap = ((inner.w() - SWATCH * PRESETS.len() as f32) / (PRESETS.len() - 1) as f32).clamp(6.0, 18.0);
        (0..PRESETS.len())
            .map(|i| {
                let x = inner.l + i as f32 * (SWATCH + gap);
                Rect::new(x, inner.t, x + SWATCH, inner.t + SWATCH)
            })
            .collect()
    }

    fn hit(&self, g: &Gfx, x: f32, y: f32) -> Hit {
        if y < HEADER {
            return Hit::None;
        }
        if y < HEADER + TABS_H {
            return form::tab_at(g, &tab_labels(), x, y).map_or(Hit::None, |k| Hit::Tab(Tab::ALL[k]));
        }
        let f = self.form();
        match f.hit(g, x, y) {
            form::Hit::Custom(C_COLORS) => {
                let Some(r) = f.custom(C_COLORS) else { return Hit::None };
                let (swatches, hue) = Self::color_rects(r);
                if let Some(i) = swatches.iter().position(|s| s.contains(x, y)) {
                    return Hit::Swatch(i);
                }
                if Rect::new(hue.l - 6.0, hue.t - 6.0, hue.r + 6.0, hue.b + 6.0).contains(x, y) {
                    return Hit::Hue(((x - hue.l) / hue.w()).clamp(0.0, 1.0));
                }
                Hit::None
            }
            form::Hit::Custom(id @ (C_BASS | C_SNARE)) => {
                let Some(r) = f.custom(id) else { return Hit::None };
                Self::pick_rects(r).iter().position(|s| s.contains(x, y)).map_or(Hit::None, |i| Hit::Pick(id, i))
            }
            form::Hit::Custom(C_SEARCH) => Hit::Search,
            form::Hit::Custom(_) => Hit::None,
            form::Hit::None => Hit::None,
            h => Hit::Form(h),
        }
    }

    fn drag_to(&mut self, x: f32) {
        let Some(id) = self.drag else { return };
        let f = self.form();
        if id == C_COLORS {
            if let Some(r) = f.custom(C_COLORS) {
                let (_, hue) = Self::color_rects(r);
                self.set_color(music::hsv(((x - hue.l) / hue.w()).clamp(0.0, 1.0) * 360.0, 1.0));
            }
            return;
        }
        if let Some(v) = f.slider_value(id, x) {
            self.slide(id, v);
        }
    }

    fn slide(&mut self, id: u32, v: f32) {
        match id {
            R_BRIGHT => self.set_brightness(v),
            R_SPEED => self.set_speed(v),
            R_SENS => {
                self.settings.sensitivity = v_to_sens(v);
                self.update_music();
                self.redraw();
            }
            R_DELAY => {
                self.settings.delay = v_to_delay(v);
                self.update_music();
                self.redraw();
            }
            _ => {}
        }
    }

    fn act(&mut self, hit: Hit) {
        use form::Hit as F;
        if hit != Hit::Search && self.search.focused {
            self.search.blur();
            self.redraw();
        }
        match hit {
            Hit::Tab(t) => self.select(t),
            Hit::Swatch(i) => {
                self.set_color(rgb(PRESETS[i].0));
                self.save();
            }
            Hit::Search => {
                self.search.focus(false);
                unsafe {
                    let _ = SetFocus(Some(self.hwnd));
                }
                self.redraw();
            }
            Hit::Form(F::Button(R_DEVICE)) => {
                if self.link.status().state == State::Connected {
                    self.link.disconnect();
                } else {
                    self.connect();
                }
            }
            Hit::Form(F::Toggle(R_AUTO) | F::Row(R_AUTO)) => {
                self.settings.auto = !self.settings.auto;
                self.save();
                self.redraw();
            }
            Hit::Form(F::Toggle(R_POWER) | F::Row(R_POWER)) => self.set_power(!self.settings.power),
            Hit::Form(F::Seg(R_DIR, k)) => self.set_reverse(k == 1),
            Hit::Form(F::Toggle(R_MUSIC) | F::Row(R_MUSIC)) => self.set_music(!self.settings.music),
            Hit::Form(F::Seg(R_REACT, k)) => {
                self.settings.react = if k == 1 { React::Flash } else { React::Colors };
                self.update_music();
                self.save();
                self.redraw();
            }
            Hit::Form(F::Seg(R_LISTEN, k)) => {
                self.settings.listen = if k == 1 { Listen::Music } else { Listen::All };
                self.update_music();
                self.save();
                self.redraw();
            }
            Hit::Pick(id, i) => {
                let c = rgb(PRESETS[i].0);
                if id == C_BASS {
                    self.settings.bass = c;
                } else {
                    self.settings.snare = c;
                }
                self.update_music();
                self.save();
                self.redraw();
            }
            Hit::Form(F::Seg(R_SOURCE, k)) => {
                self.settings.source = if k == 0 { Source::Beat } else { Source::Drums };
                self.update_music();
                self.save();
                self.redraw();
            }
            Hit::Form(F::Row(id)) if id > R_EFFECT => {
                if self.settings.music {
                    self.set_music(false);
                }
                self.set_effect((id - R_EFFECT) as u16);
            }
            _ => {}
        }
    }

    // --- Çizim ---

    pub fn paint(&self, g: &Gfx) {
        let f = self.form();
        g.clip(Rect::new(0.0, HEADER + TABS_H, self.w, self.h), || {
            f.paint(g, self.hover.form(), self.drag);
            if let Some(r) = f.custom(C_COLORS) {
                self.paint_colors(g, r);
            }
            if let Some(r) = f.custom(C_SEARCH) {
                self.search.paint(g, r, t!("Search effects (name or number)", "Efektlerde ara (ad ya da numara)"));
            }
            if let Some(r) = f.custom(C_METER) {
                self.paint_meter(g, r);
            }
            let flash = self.settings.react == React::Flash;
            if let Some(r) = f.custom(C_BASS) {
                self.paint_pick(g, r, C_BASS, if flash { t!("Color", "Renk") } else { t!("Kick", "Bas") }, self.settings.bass);
            }
            if let Some(r) = f.custom(C_SNARE) {
                self.paint_pick(g, r, C_SNARE, t!("Snare", "Trampet"), self.settings.snare);
            }
        });
        let hover = Tab::ALL.iter().position(|&t| self.hover == Hit::Tab(t));
        form::paint_tabs(g, &tab_labels(), self.tab as usize, hover, self.w);
    }

    fn card(g: &Gfx, r: Rect) {
        g.fill(r, 8.0, pal().panel);
        g.stroke(r, 8.0, pal().line, 1.0);
    }

    /// Hazır renkler (seçili olanın çevresinde halka) ve ton şeridi (seçili tonda işaret).
    fn paint_colors(&self, g: &Gfx, r: Rect) {
        Self::card(g, r);
        let (swatches, hue) = Self::color_rects(r);
        let current = hex(self.settings.color);
        let plain = self.settings.effect == 0;
        for (i, s) in swatches.iter().enumerate() {
            let (cx, cy) = (s.l + SWATCH / 2.0, s.cy());
            let c = PRESETS[i].0;
            if plain && c == current {
                g.ring(cx, cy, SWATCH / 2.0 + 3.0, pal().text, 2.0);
            } else if self.hover == Hit::Swatch(i) {
                g.ring(cx, cy, SWATCH / 2.0 + 3.0, pal().faint, 1.5);
            }
            g.circle(cx, cy, SWATCH / 2.0 - 1.0, Color::rgb(c));
        }
        let stops: Vec<(f32, Color)> = (0..=6).map(|k| (k as f32 / 6.0, Color::rgb(hex(music::hsv(k as f32 * 60.0, 1.0))))).collect();
        g.linear(hue, hue.h() / 2.0, (hue.l, hue.cy()), (hue.r, hue.cy()), &stops);
        // Seçili renk tam doygun bir tonsa şeritte yeri işaretlenir.
        if plain && let Some(h) = pure_hue(self.settings.color) {
            let x = hue.l + h / 360.0 * hue.w();
            g.circle(x, hue.cy(), hue.h() / 2.0 + 2.0, pal().text);
            g.circle(x, hue.cy(), hue.h() / 2.0 - 2.0, Color::rgb(current));
        }
    }

    /// Müzikteki renk seçicisi: solda adı, sağda hazır renkler (seçilinin çevresinde halka).
    fn paint_pick(&self, g: &Gfx, r: Rect, id: u32, label: &str, current: [u8; 3]) {
        Self::card(g, r);
        g.text(label, &g.f.body, Rect::new(r.l + 16.0, r.t, r.l + 16.0 + PICK_LABEL, r.b), pal().text);
        let current = hex(current);
        for (i, s) in Self::pick_rects(r).iter().enumerate() {
            let (cx, cy) = (s.l + SWATCH / 2.0, s.cy());
            let c = PRESETS[i].0;
            if c == current {
                g.ring(cx, cy, SWATCH / 2.0 + 3.0, pal().text, 2.0);
            } else if self.hover == Hit::Pick(id, i) {
                g.ring(cx, cy, SWATCH / 2.0 + 3.0, pal().faint, 1.5);
            }
            g.circle(cx, cy, SWATCH / 2.0 - 1.0, Color::rgb(c));
        }
    }

    /// Canlı ölçer: ritim, bas, trampet ve zil için vuruşta yanan dört nokta; sağda BPM.
    fn paint_meter(&self, g: &Gfx, r: Rect) {
        Self::card(g, r);
        let Some(m) = self.music.as_ref().map(Music::meter) else { return };
        let glow = |t: Option<std::time::Instant>| {
            let age = t.map(|t| t.elapsed()).unwrap_or(Duration::from_secs(9));
            (1.0 - age.as_secs_f32() / 0.25).clamp(0.0, 1.0)
        };
        let dots = [
            (t!("Beat", "Ritim"), m.beat, pal().text),
            (t!("Kick", "Bas"), m.hits[0], Color::rgb(0xFF4D4D)),
            (t!("Snare", "Trampet"), m.hits[1], Color::rgb(0x4DA3FF)),
            (t!("Hi-hat", "Zil"), m.hits[2], Color::rgb(0x5BE07A)),
        ];
        let span = ((r.w() - 140.0) / dots.len() as f32).min(96.0);
        for (i, (label, at, c)) in dots.into_iter().enumerate() {
            let cx = r.l + 28.0 + span / 2.0 + i as f32 * span;
            let cy = r.t + 42.0;
            g.circle(cx, cy, 18.0, pal().hover);
            let v = glow(at);
            if v > 0.0 {
                g.circle(cx, cy, 8.0 + 10.0 * v, c.alpha(v));
            }
            g.text(label, &g.f.small, Rect::new(cx - span / 2.0, cy + 26.0, cx + span / 2.0, cy + 46.0), pal().muted);
        }
        let bpm = if m.bpm > 0.0 && m.sound { format!("{:.0}", m.bpm) } else { "–".into() };
        g.text(&bpm, &g.f.heading, Rect::new(r.r - 104.0, r.t + 22.0, r.r - 16.0, r.cy() + 8.0), pal().text);
        let sub = if m.sound { "BPM" } else { t!("No sound", "Ses yok") };
        g.text(sub, &g.f.small, Rect::new(r.r - 104.0, r.cy() + 8.0, r.r - 16.0, r.b - 18.0), pal().muted);
    }
}

/// "#20E040", "20e040" ya da hazır rengin adı (Türkçe ya da İngilizce; büyük küçük harf ve ı/i fark
/// etmez).
fn parse_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6
        && let Ok(v) = u32::from_str_radix(h, 16)
    {
        return Some(rgb(v));
    }
    let fold = |t: &str| t.trim().to_lowercase().replace('ı', "i").replace('\u{307}', "");
    let f = fold(s);
    PRESETS.iter().find(|p| fold(p.1) == f || fold(p.2) == f).map(|p| rgb(p.0))
}

/// Tam doygun ve tam parlak bir renkse tonu.
fn pure_hue(c: [u8; 3]) -> Option<f32> {
    let max = *c.iter().max()?;
    let min = *c.iter().min()?;
    if max != 255 || min != 0 {
        return None;
    }
    (0..360).map(|h| h as f32).find(|h| music::hsv(*h, 1.0).iter().zip(c).all(|(a, b)| a.abs_diff(b) <= 3))
}

impl ToolPage for Lights {
    fn summary(&self) -> Option<(String, String)> {
        let s = self.link.status();
        let top = match s.state {
            State::Connected => s.name.clone(),
            State::Searching | State::Connecting => t!("Connecting…", "Bağlanıyor…").into(),
            State::Off => t!("Not connected", "Bağlı değil").into(),
        };
        let st = &self.settings;
        let what = if st.music {
            t!("With the music", "Müzikle").to_string()
        } else if !st.power {
            t!("Off", "Kapalı").into()
        } else if st.effect > 0 {
            self.model().effects.iter().find(|e| e.id == st.effect).map(surucu::effect_name).unwrap_or_default()
        } else {
            PRESETS
                .iter()
                .find(|p| p.0 == hex(st.color))
                .map_or_else(|| format!("#{:06X}", hex(st.color)), |p| t!(p.1, p.2).to_string())
        };
        Some((top, format!("{what} · {}", pct(st.brightness))))
    }

    fn message(&mut self, _g: &mut Gfx, msg: u32, wp: WPARAM, _lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_LINK => self.on_link(),
            WM_ASK => {
                let asks: Vec<_> = self.asks.try_iter().collect();
                for r in asks {
                    let a = self.answer(&r.body);
                    r.reply(a);
                }
            }
            WM_TIMER if wp.0 == TIMER_METER => self.redraw(),
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn layout(&mut self, w: f32, h: f32, _head_x: f32, _head_r: f32) {
        self.w = w;
        self.h = h;
        let k = self.tab as usize;
        self.scroll[k] = self.scroll[k].min(self.max_scroll());
    }

    fn paint(&self, g: &Gfx) {
        Lights::paint(self, g)
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
        match self.pressed {
            Hit::Form(form::Hit::Slider(id, v)) => {
                self.drag = Some(id);
                self.slide(id, v);
            }
            Hit::Hue(v) => {
                self.drag = Some(C_COLORS);
                self.set_color(music::hsv(v * 360.0, 1.0));
            }
            _ => {}
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
        self.act(hit);
    }

    fn wheel(&mut self, g: &Gfx, x: f32, y: f32, delta: f32) {
        let step = delta.signum() * 0.05;
        match self.hit(g, x, y) {
            Hit::Form(form::Hit::Slider(R_BRIGHT, _)) => self.set_brightness((self.settings.brightness + step).clamp(0.0, 1.0)),
            Hit::Form(form::Hit::Slider(R_SPEED, _)) => self.set_speed((self.settings.speed + step).clamp(0.0, 1.0)),
            Hit::Form(form::Hit::Slider(R_SENS, _)) => {
                self.slide(R_SENS, (sens_to_v(self.settings.sensitivity) + step).clamp(0.0, 1.0));
            }
            Hit::Form(form::Hit::Slider(R_DELAY, _)) => {
                self.slide(R_DELAY, (delay_to_v(self.settings.delay) + step).clamp(0.0, 1.0));
            }
            _ => {
                let k = self.tab as usize;
                self.scroll[k] = (self.scroll[k] - delta * 60.0).clamp(0.0, self.max_scroll());
                self.hover = self.hit(g, x, y);
                self.redraw();
                return;
            }
        }
        self.save();
    }

    /// Arama kutusu odaktayken tuşlar ona; Ctrl+F efekt aramasını açar.
    fn key(&mut self, vk: u16) -> bool {
        let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
        if ctrl && VIRTUAL_KEY(vk) == VK_F {
            self.select(Tab::Effects);
            self.search.focus(true);
            self.redraw();
            return true;
        }
        match self.search.key(vk, ctrl) {
            FieldKey::Ignored => false,
            FieldKey::Enter | FieldKey::Escape => {
                if VIRTUAL_KEY(vk) == VK_ESCAPE {
                    self.search.set("");
                }
                self.search.blur();
                self.scroll[Tab::Effects as usize] = 0.0;
                self.redraw();
                true
            }
            FieldKey::Handled => {
                self.scroll[Tab::Effects as usize] = 0.0;
                self.redraw();
                true
            }
        }
    }

    fn char(&mut self, c: char) -> bool {
        if !self.search.char(c) {
            return false;
        }
        self.scroll[Tab::Effects as usize] = 0.0;
        self.redraw();
        true
    }

    fn interactive(&self, g: &Gfx, x: f32, y: f32) -> bool {
        self.hit(g, x, y) != Hit::None
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.update_timer();
        if !visible {
            self.hover = Hit::None;
            self.drag = None;
            self.search.blur();
        }
    }

    fn kill_focus(&mut self) {
        if self.search.focused {
            self.search.blur();
            self.redraw();
        }
    }
}

impl Drop for Lights {
    fn drop(&mut self) {
        ortak::hub::close("lights");
        // Önce müzik (bağlantıya komut gönderen), sonra bağlantı.
        self.music = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_METER);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renk_adlari() {
        assert_eq!(parse_color("#20E040"), Some([0x20, 0xE0, 0x40]));
        assert_eq!(parse_color("ff0000"), Some([255, 0, 0]));
        assert_eq!(parse_color("Yeşil"), Some(rgb(0x20E040)));
        assert_eq!(parse_color("KIRMIZI"), Some(rgb(0xFF2020)));
        assert_eq!(parse_color("sicak beyaz"), Some(rgb(0xFFB070)));
        assert_eq!(parse_color("blue"), Some(rgb(0x1060FF)));
        assert_eq!(parse_color("lacivert"), None);
    }
}

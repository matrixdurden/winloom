//! Müzikle yanan ışık: varsayılan çıkış cihazının sesi (WASAPI loopback) dinlenir, şarkının
//! temposu ve davul vuruşları bulunur, ışığa komut gider.
//!
//! Bulma (`Detector`), CapCut'ın ritim bulmasındaki yöntemin akan sese uyarlanmışı: 2048 örneklik
//! pencere 512'şer kayar (~11 ms); üç bantta (bas 30–150 Hz, orta 150–4000, tiz 6–16 kHz)
//! logaritmik genliğin artışı ölçülür (SuperFlux: iki kare öncesinin frekansta komşularıyla en
//! büyüğünden fark, sürekli notalar ve vokal titreşimi sayılmasın). Bandın son 3 saniyedeki
//! ortalamasına göre tepe yapan kare vuruştur; türü en baskın bant: bas → kick, orta → trampet,
//! tiz → zil. Ses düzeyinden bağımsızdır.
//!
//! Tempo: üç bandın toplamı son 6 saniyede kendisiyle kaydırılarak karşılaştırılır
//! (otokorelasyon, 60–200 BPM, 120 civarı tercih); faz, vuruşların en çok üst üste geldiği yer.
//! Sonraki vuruş tahmin edilir; komut ışığa varana dek geçen süre kadar önden gönderilir.
//!
//! İki mod: Ritim (tempo ızgarasının her vuruşunda yeni renk) ve Davul (kick, trampet ve zil
//! ayrı renk; ızgaraya oturmayan zayıf vuruşlar, 808 basın nota değişimleri gibi, elenir).
//! Bluetooth kulaklıkta ses geç geldiği için bütün komutlar istenen kadar geciktirilebilir.
//!
//! Ses yokken loopback paket göndermez; ölçer söner. Varsayılan cihaz değişirse yenisine geçilir.

use std::collections::VecDeque;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};

use crate::link::Tx;
use crate::surucu::Cmd;

/// Pencere ve kayma (örnek).
const N: usize = 2048;
const HOP: usize = 512;
/// Bantlar (Hz): bas, orta, tiz.
const BANDS: [(f32, f32); 3] = [(30.0, 150.0), (150.0, 4000.0), (6000.0, 16000.0)];
/// Bantların ölçeği bu kadar geriye bakar (sn).
const SCALE_S: f32 = 3.0;
/// Tempo bu kadar geriye bakar (sn) ve bu aralıkla yeniden hesaplanır.
const TEMPO_S: f32 = 6.0;
const TEMPO_EVERY_S: f32 = 0.5;
/// Aynı banttaki iki vuruş arası en az (sn) ve iki vuruş (her tür) arası en az.
const MIN_GAP: [f64; 3] = [0.10, 0.10, 0.07];
const HIT_GAP: f64 = 0.05;
/// Vuruş, akının tepe yaptığı karenin bitişinden bu kadar önce olmuştur (sn; ölçülen).
const ONSET_LAG: f64 = 0.017;
/// Tempo zarfında bantların ağırlığı: ritmi kick ve trampet taşır, zil sık ve hafif.
const TEMPO_WEIGHT: [f32; 3] = [1.0, 0.6, 0.15];
/// Bu kadar süre vuruş yoksa (sn) şarkı durmuş sayılır: ritim vuruşları da durur.
const QUIET_S: f64 = 2.5;
/// Komutun ışığa varma süresi: tahmin edilen vuruş bu kadar önden gönderilir.
const LEAD: Duration = Duration::from_millis(40);
/// Varsayılan cihaz bu aralıkla yeniden sorulur.
const DEVICE_CHECK: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum React {
    /// Her vuruşta renk değişir.
    Colors,
    /// Loş bekler, her vuruşta parlar.
    Flash,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Şarkının tempo ızgarası: her vuruşta (CapCut'ın ritim işaretleri gibi).
    Beat,
    /// Davul vuruşları: kick, trampet ve zil ayrı renk.
    Drums,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Kick,
    Snare,
    Hat,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub react: React,
    pub source: Source,
    /// Eşik çarpanı: küçüldükçe daha hafif vuruşlar da sayılır.
    pub sensitivity: f32,
    /// Tepe parlaklık (kullanıcının parlaklığı).
    pub brightness: f32,
    /// Işık bu kadar geciktirilir (Bluetooth kulaklıkta ses geç gelir).
    pub delay: Duration,
}

/// Sayfadaki ölçer için son durum.
#[derive(Clone, Copy, Default)]
pub struct Meter {
    pub bpm: f32,
    /// Son ritim vuruşu; son kick, trampet, zil.
    pub beat: Option<Instant>,
    pub hits: [Option<Instant>; 3],
    /// Ses geliyor mu.
    pub sound: bool,
}

pub struct Music {
    cfg: Arc<Mutex<Config>>,
    meter: Arc<Mutex<Meter>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Music {
    pub fn start(tx: Tx, cfg: Config) -> Self {
        let cfg = Arc::new(Mutex::new(cfg));
        let meter = Arc::new(Mutex::new(Meter::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, m, s) = (cfg.clone(), meter.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("lights-muzik".into())
            .spawn(move || {
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                // Kilit tek seferde alınıp bırakılır: aynı ifadede iki kez almak iş parçacığını
                // kendi kendine kilitler (sayfa da ayara dokununca onu bekleyip donar).
                let first = *c.lock().unwrap();
                let mut show = Show::new(tx, first);
                while !s.load(Ordering::Relaxed) {
                    if let Err(e) = unsafe { capture(&c, &m, &s, &mut show) } {
                        ortak::log!("lights: ses yakalanamadı · {}", e.message());
                        *m.lock().unwrap() = Meter::default();
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
                unsafe {
                    CoUninitialize();
                }
            })
            .ok();
        ortak::log!("lights: müzik modu başladı");
        Self { cfg, meter, stop, thread }
    }

    pub fn set(&self, cfg: Config) {
        *self.cfg.lock().unwrap() = cfg;
    }

    pub fn meter(&self) -> Meter {
        *self.meter.lock().unwrap()
    }
}

impl Drop for Music {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        ortak::log!("lights: müzik modu durdu");
    }
}

/// Ton (derece) ve doygunluktan tam parlak RGB.
pub fn hsv(h: f32, s: f32) -> [u8; 3] {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = 1.0 - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [((r + m) * 255.0).round() as u8, ((g + m) * 255.0).round() as u8, ((b + m) * 255.0).round() as u8]
}

// --- Bulma ---

/// Bulunan davul vuruşu: türü, gücü (bandının ortalamasına oranı) ve anı (ses saatinde, sn).
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub kind: Kind,
    pub strength: f32,
    pub at: f64,
}

/// Akan sesten davul vuruşları ve tempo. Zaman, işlenen örneklerden sayılan ses saatidir (sn).
pub struct Detector {
    fr: f32,
    window: Vec<f32>,
    bins: [(usize, usize); 3],
    /// İki ve bir kare önceki logaritmik genlik.
    prev: [Vec<f32>; 2],
    /// Bantların akı geçmişi ve son karelerde ortalamaya oranı.
    flux: [VecDeque<f32>; 3],
    norm: [VecDeque<f32>; 3],
    last_band: [f64; 3],
    last_hit: f64,
    /// Tempo için üç bandın toplamı.
    env: VecDeque<f32>,
    frames: u64,
    /// Vuruş aralığı (kare) ve sonraki vuruşun ses saati.
    period: Option<f32>,
    next_beat: Option<f64>,
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Detector {
    pub fn new(rate: f32) -> Self {
        let window = (0..N).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (N - 1) as f32).cos()).collect();
        let bin = |f: f32| ((f * N as f32 / rate).ceil() as usize).min(N / 2);
        let bins = BANDS.map(|(lo, hi)| (bin(lo), bin(hi).max(bin(lo) + 1)));
        Detector {
            fr: rate / HOP as f32,
            window,
            bins,
            prev: [Vec::new(), Vec::new()],
            flux: Default::default(),
            norm: Default::default(),
            last_band: [f64::MIN; 3],
            last_hit: f64::MIN,
            env: VecDeque::new(),
            frames: 0,
            period: None,
            next_beat: None,
            re: vec![0.0; N],
            im: vec![0.0; N],
        }
    }

    pub fn bpm(&self) -> f32 {
        self.period.map_or(0.0, |p| 60.0 * self.fr / p)
    }

    /// Şarkı çalıyor mu (son birkaç saniyede vuruş oldu mu).
    pub fn playing(&self, now: f64) -> bool {
        now - self.last_hit < QUIET_S
    }

    /// Sonraki ritim vuruşu (ses saati).
    pub fn next_beat(&self) -> Option<f64> {
        self.next_beat
    }

    /// Tahmin edilen vuruş geçildi: sıradakine.
    pub fn advance_beat(&mut self) {
        if let (Some(b), Some(p)) = (self.next_beat.as_mut(), self.period) {
            *b += (p / self.fr) as f64;
        }
    }

    /// `at` vuruş ızgarasının 16'lık bir yerine yakın mı (tempo yoksa her yer yakın).
    pub fn on_grid(&self, at: f64) -> bool {
        let (Some(b), Some(p)) = (self.next_beat, self.period) else { return true };
        let step = (p / self.fr / 4.0) as f64;
        let off = (at - b).rem_euclid(step);
        off.min(step - off) < 0.045
    }

    /// Yeni pencere (`x`, `N` örnek; son örneği `now` anında). Bir kare önce vuruş varsa onu
    /// döndürür; `threshold`: vuruş sayılması için gereken en az güç.
    pub fn frame(&mut self, x: &[f32], now: f64, threshold: f32) -> Option<Hit> {
        for ((re, &x), &w) in self.re.iter_mut().zip(&x[..N]).zip(&self.window) {
            *re = x * w;
        }
        self.im.fill(0.0);
        fft(&mut self.re, &mut self.im);
        let scale = 1000.0 / (N as f32 / 2.0);
        let level: Vec<f32> = (0..=N / 2).map(|k| (1.0 + scale * (self.re[k] * self.re[k] + self.im[k] * self.im[k]).sqrt()).ln()).collect();
        if self.prev[0].is_empty() {
            self.prev = [level.clone(), level];
            return None;
        }
        let old = &self.prev[0];
        let keep = (self.fr * SCALE_S) as usize;
        let mut env = 0.0;
        for (b, &(lo, hi)) in self.bins.iter().enumerate() {
            let mut sum = 0.0;
            for k in lo..hi {
                let near = old[k].max(old[k.saturating_sub(1)]).max(old[(k + 1).min(N / 2)]);
                sum += (level[k] - near).max(0.0);
            }
            let f = sum / (hi - lo) as f32;
            let h = &mut self.flux[b];
            h.push_back(f);
            if h.len() > keep {
                h.pop_front();
            }
            let mean = h.iter().sum::<f32>() / h.len() as f32 + 1e-6;
            let n = &mut self.norm[b];
            n.push_back(f / mean);
            if n.len() > 3 {
                n.pop_front();
            }
            env += f / mean * TEMPO_WEIGHT[b];
        }
        self.prev = [std::mem::take(&mut self.prev[1]), level];
        self.env.push_back(env);
        if self.env.len() > (self.fr * TEMPO_S) as usize {
            self.env.pop_front();
        }
        self.frames += 1;
        if self.frames.is_multiple_of((self.fr * TEMPO_EVERY_S) as u64) {
            self.tempo(now - ONSET_LAG);
        }
        self.pick(now - 1.0 / self.fr as f64 - ONSET_LAG, threshold)
    }

    /// Bir kare önceki tepe: hangi bantlarda vuruş var, en baskını hangisi.
    fn pick(&mut self, at: f64, threshold: f32) -> Option<Hit> {
        let mut any = false;
        let mut strength = [0f32; 3];
        for b in 0..3 {
            let h = &self.flux[b];
            let len = h.len();
            if len < 14 {
                return None;
            }
            let v = h[len - 2];
            let before = h.range(len - 6..len - 2).fold(0f32, |a, &x| a.max(x));
            let avg = h.range(len - 14..len - 2).sum::<f32>() / 12.0;
            let mean = h.iter().sum::<f32>() / len as f32;
            // Güç: tepe karesi ve komşularının en büyüğü (bantlar bir kare kayabilir).
            strength[b] = self.norm[b].iter().fold(0f32, |a, &x| a.max(x));
            if v >= before && v > h[len - 1] && v > avg + 2.0 * mean && at - self.last_band[b] > MIN_GAP[b] {
                self.last_band[b] = at;
                any = true;
            }
        }
        if !any {
            return None;
        }
        let (b, s) = strength.iter().copied().enumerate().fold((0, 0f32), |a, (i, s)| if s > a.1 { (i, s) } else { a });
        if s < threshold || at - self.last_hit < HIT_GAP {
            return None;
        }
        self.last_hit = at;
        Some(Hit { kind: [Kind::Kick, Kind::Snare, Kind::Hat][b], strength: s, at })
    }

    /// Tempo ve faz: otokorelasyonun en iyi aralığı (120 BPM civarı tercih), o aralıkta
    /// vuruşların en çok üst üste geldiği faz. Tahmin yumuşakça düzeltilir.
    fn tempo(&mut self, now: f64) {
        let n = self.env.len();
        if n < (self.fr * 4.0) as usize {
            return;
        }
        let mean = self.env.iter().sum::<f32>() / n as f32;
        let e: Vec<f32> = self.env.iter().map(|v| v - mean).collect();
        let (lo, hi) = ((self.fr * 60.0 / 200.0) as usize, (self.fr * 60.0 / 60.0) as usize);
        let r: Vec<f32> = (lo..=hi).map(|l| e[..n - l].iter().zip(&e[l..]).map(|(a, b)| a * b).sum::<f32>() / (n - l) as f32).collect();
        let fr = self.fr;
        let score = |i: usize| {
            let bpm = 60.0 * fr / (lo + i) as f32;
            r[i] * (-0.5 * (bpm / 120.0).log2().powi(2)).exp()
        };
        let Some(k) = (0..r.len()).max_by(|&a, &b| score(a).total_cmp(&score(b))) else { return };
        if r[k] <= 0.0 {
            return;
        }
        let mut lag = (lo + k) as f32;
        if k > 0 && k + 1 < r.len() {
            let (a, b, c) = (r[k - 1], r[k], r[k + 1]);
            let den = a - 2.0 * b + c;
            if den != 0.0 {
                lag += 0.5 * (a - c) / den;
            }
        }
        // Faz: son 4 saniyede her aralıkta bir kare toplanır; en büyük toplamın kayması.
        let count = (self.fr * 4.0 / lag) as usize;
        let best = (0..lag.ceil() as usize)
            .map(|ph| {
                let s: f32 = (0..count)
                    .filter_map(|j| {
                        let t = n as f32 - 1.0 - ph as f32 - j as f32 * lag;
                        (t >= 0.0).then(|| e[t.round() as usize])
                    })
                    .sum();
                (ph, s)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(0, |b| b.0);
        let per = (lag / self.fr) as f64;
        let mut cand = now - best as f64 / self.fr as f64 + per;
        while cand < now {
            cand += per;
        }
        match (self.period, self.next_beat) {
            (Some(p), Some(next)) if (lag - p).abs() / p <= 0.08 => {
                let p = 0.8 * p + 0.2 * lag;
                self.period = Some(p);
                let per = (p / self.fr) as f64;
                let err = (cand - next + per / 2.0).rem_euclid(per) - per / 2.0;
                self.next_beat = Some(next + 0.5 * err);
            }
            _ => {
                self.period = Some(lag);
                self.next_beat = Some(cand);
            }
        }
    }
}

// --- Işık ---

#[derive(Clone, Copy, Debug)]
enum Event {
    Beat,
    Hit(Kind),
}

/// Vuruşlara tepki: komutları zamanında gönderir.
struct Show {
    tx: Tx,
    cfg: Config,
    /// Paletin tonu: kick iki komşu tonu sırayla, trampet karşı tonu kullanır.
    hue: f32,
    kick_alt: bool,
    beats: u32,
    /// Bekleyen işler (gecikme ve önden gönderme için), zamana göre sıralı.
    queue: VecDeque<(Instant, Event)>,
    /// Flaştan (ya da zil kırpışmasından) dönüş anı.
    dim_at: Option<Instant>,
}

impl Show {
    fn new(tx: Tx, cfg: Config) -> Self {
        let mut s = Show { tx, cfg, hue: 0.0, kick_alt: false, beats: 0, queue: VecDeque::new(), dim_at: None };
        s.enter();
        s
    }

    fn low(&self) -> f32 {
        (self.cfg.brightness * 0.12).max(0.02)
    }

    fn rest(&self) -> f32 {
        match self.cfg.react {
            React::Colors => self.cfg.brightness,
            React::Flash => self.low(),
        }
    }

    /// Tepki türüne girerken ışığın başlangıç hali.
    fn enter(&mut self) {
        self.dim_at = None;
        self.tx.send(Cmd::Brightness(self.rest()));
        self.tx.send(Cmd::Color(hsv(self.hue, 1.0)));
    }

    fn config(&mut self, cfg: Config) {
        let changed = cfg.react != self.cfg.react || cfg.brightness != self.cfg.brightness || cfg.source != self.cfg.source;
        self.cfg = cfg;
        if changed {
            self.queue.clear();
            self.enter();
        }
    }

    /// `at` anında yapılacak iş (gecikme eklenir).
    fn schedule(&mut self, at: Instant, e: Event) {
        let at = at + self.cfg.delay;
        let i = self.queue.iter().position(|(t, _)| *t > at).unwrap_or(self.queue.len());
        self.queue.insert(i, (at, e));
    }

    fn tick(&mut self, now: Instant) {
        while self.queue.front().is_some_and(|(t, _)| *t <= now) {
            let (_, e) = self.queue.pop_front().unwrap();
            self.play(e, now);
        }
        if self.dim_at.is_some_and(|t| now >= t) {
            self.dim_at = None;
            self.tx.send(Cmd::Brightness(self.rest()));
        }
    }

    fn flash(&mut self, level: f32, ms: u64, now: Instant) {
        self.tx.send(Cmd::Brightness(self.cfg.brightness * level));
        self.dim_at = Some(now + Duration::from_millis(ms));
    }

    fn play(&mut self, e: Event, now: Instant) {
        let flash = self.cfg.react == React::Flash;
        match e {
            // Ritim: her vuruşta altın açıyla ilerleyen ton; art arda renkler hep belirgin farklı.
            Event::Beat => {
                self.hue = (self.hue + 137.5) % 360.0;
                self.tx.send(Cmd::Color(hsv(self.hue, 1.0)));
                if flash {
                    self.flash(1.0, 140, now);
                }
            }
            Event::Hit(Kind::Kick) => {
                self.kick_alt = !self.kick_alt;
                let h = self.hue + if self.kick_alt { 0.0 } else { 35.0 };
                self.tx.send(Cmd::Color(hsv(h, 1.0)));
                if flash {
                    self.flash(1.0, 140, now);
                }
            }
            Event::Hit(Kind::Snare) => {
                self.tx.send(Cmd::Color(hsv(self.hue + 180.0, 1.0)));
                if flash {
                    self.flash(0.85, 110, now);
                }
            }
            // Zil rengi değiştirmez: kısa bir kırpışma (başka bir flaşın üstüne binmez).
            Event::Hit(Kind::Hat) => {
                if self.dim_at.is_none() {
                    if flash {
                        self.flash(0.4, 50, now);
                    } else {
                        self.flash(0.45, 60, now);
                    }
                }
            }
        }
    }

    /// Davul modunda palet iki ölçüde (8 vuruş) bir ilerler.
    fn beat_passed(&mut self) {
        self.beats += 1;
        if self.cfg.source == Source::Drums && self.beats.is_multiple_of(8) {
            self.hue = (self.hue + 70.0) % 360.0;
        }
    }
}

/// Varsayılan çıkış cihazını dinler; cihaz değişince ya da durdurulunca döner.
unsafe fn capture(cfg: &Mutex<Config>, meter: &Mutex<Meter>, stop: &AtomicBool, show: &mut Show) -> windows::core::Result<()> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let id = device.GetId()?;
        let device_id = id.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id.0 as *const _));

        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let fmt = *format;
        let channels = fmt.nChannels.max(1) as usize;
        let rate = fmt.nSamplesPerSec.max(1) as f32;
        let bits = fmt.wBitsPerSample;
        let float = bits == 32 && {
            let tag = fmt.wFormatTag as u32;
            tag == WAVE_FORMAT_IEEE_FLOAT
                || (tag == WAVE_FORMAT_EXTENSIBLE
                    && std::ptr::addr_of!((*(format as *const WAVEFORMATEXTENSIBLE)).SubFormat).read_unaligned() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT)
        };
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 200_000, 0, format, None);
        CoTaskMemFree(Some(format as *const _));
        init?;
        if !float && bits != 16 {
            return Err(windows::core::Error::new(windows::core::HRESULT(-1), format!("{bits} bit ses")));
        }
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;

        let mut det = Detector::new(rate);
        let mut buf: Vec<f32> = Vec::with_capacity(N * 2);
        // Ses saati: işlenen örnek sayısı.
        let mut done: u64 = 0;
        let mut last_packet = Instant::now();
        let mut last_check = Instant::now();

        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            std::thread::sleep(Duration::from_millis(4));
            let now = Instant::now();
            let c = *cfg.lock().unwrap();
            show.config(c);

            if now - last_check >= DEVICE_CHECK {
                last_check = now;
                let current = enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .and_then(|d| d.GetId())
                    .map(|p| {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as *const _));
                        s
                    })
                    .unwrap_or_default();
                if current != device_id {
                    ortak::log!("lights: varsayılan ses cihazı değişti");
                    break Ok(());
                }
            }

            let mut got = false;
            loop {
                let size = match cap.GetNextPacketSize() {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = client.Stop();
                        return Err(e);
                    }
                };
                if size == 0 {
                    break;
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                for f in 0..frames as usize {
                    let mut s = 0.0;
                    if !silent {
                        for ch in 0..channels {
                            let i = f * channels + ch;
                            s += if float { *(data as *const f32).add(i) } else { *(data as *const i16).add(i) as f32 / 32768.0 };
                        }
                    }
                    buf.push(s / channels as f32);
                }
                cap.ReleaseBuffer(frames)?;
                got = true;
            }

            // Ses saatinden duvar saatine: en son gelen örnek şu an çaldı sayılır.
            let end = (done + buf.len() as u64) as f64 / rate as f64;
            let wall = |t: f64| {
                let ago = end - t;
                if ago >= 0.0 { now.checked_sub(Duration::from_secs_f64(ago)).unwrap_or(now) } else { now + Duration::from_secs_f64(-ago) }
            };
            while buf.len() >= N {
                let t = (done + N as u64) as f64 / rate as f64;
                if let Some(hit) = det.frame(&buf[..N], t, c.sensitivity * 2.0) {
                    meter.lock().unwrap().hits[hit.kind as usize] = Some(wall(hit.at));
                    // Davul: ızgaraya oturmayan zayıf vuruşlar elenir.
                    if c.source == Source::Drums && (hit.strength >= c.sensitivity * 4.0 || det.on_grid(hit.at)) {
                        show.schedule(wall(hit.at), Event::Hit(hit.kind));
                    }
                }
                buf.drain(..HOP);
                done += HOP as u64;
            }
            // Ritim vuruşu: tahmin edilen an, komutun ışığa varma süresi kadar önden.
            if let Some(b) = det.next_beat()
                && wall(b) <= now + LEAD
            {
                if det.playing(end) {
                    if c.source == Source::Beat {
                        show.schedule(wall(b).checked_sub(LEAD).unwrap_or(now), Event::Beat);
                    }
                    show.beat_passed();
                    meter.lock().unwrap().beat = Some(wall(b));
                }
                det.advance_beat();
            }
            show.tick(now);

            let mut m = meter.lock().unwrap();
            m.bpm = if det.playing(end) { det.bpm() } else { 0.0 };
            if got {
                last_packet = now;
                m.sound = true;
            } else if now - last_packet > Duration::from_millis(300) {
                m.sound = false;
            }
        };
        let _ = client.Stop();
        result
    }
}

/// Yerinde, özyinelemesiz radix-2 FFT (duvar kâğıdı motorundakiyle aynı).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                (cr, ci) = (cr * wr - ci * wi, cr * wi + ci * wr);
            }
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renkler() {
        assert_eq!(hsv(0.0, 1.0), [255, 0, 0]);
        assert_eq!(hsv(120.0, 1.0), [0, 255, 0]);
        assert_eq!(hsv(240.0, 1.0), [0, 0, 255]);
        assert_eq!(hsv(60.0, 0.0), [255, 255, 255]);
    }

    /// Geliştirme aracı: bilgisayarda çalanı `KAYIT` yoluna `KAYIT_SN` saniye mono WAV olarak
    /// kaydeder (vuruş bulmayı gerçek şarkıyla ayarlamak için).
    /// `KAYIT=C:\\...\\kayit.wav KAYIT_SN=60 cargo test -p lights kaydet -- --ignored`
    #[test]
    #[ignore]
    fn kaydet() {
        let path = std::env::var("KAYIT").expect("KAYIT=yol");
        let secs: f32 = std::env::var("KAYIT_SN").ok().and_then(|v| v.parse().ok()).unwrap_or(30.0);
        let mut samples: Vec<f32> = Vec::new();
        let rate: u32;
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap();
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).unwrap();
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
            let format = client.GetMixFormat().unwrap();
            let fmt = *format;
            let bits = fmt.wBitsPerSample;
            assert_eq!(bits, 32, "32 bit kayan nokta bekleniyor");
            let channels = fmt.nChannels as usize;
            rate = fmt.nSamplesPerSec;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 200_000, 0, format, None).unwrap();
            let cap: IAudioCaptureClient = client.GetService().unwrap();
            client.Start().unwrap();
            let want = (secs * rate as f32) as usize;
            let start = Instant::now();
            while samples.len() < want {
                std::thread::sleep(Duration::from_millis(5));
                while cap.GetNextPacketSize().unwrap() > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None).unwrap();
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    for f in 0..frames as usize {
                        let mut v = 0.0;
                        if !silent {
                            for ch in 0..channels {
                                v += *(data as *const f32).add(f * channels + ch);
                            }
                        }
                        samples.push(v / channels as f32);
                    }
                    cap.ReleaseBuffer(frames).unwrap();
                }
                // Sessizlikte paket gelmez: süre dolunca bitir.
                if start.elapsed().as_secs_f32() > secs + 5.0 {
                    break;
                }
            }
            let _ = client.Stop();
        }
        let mut out = Vec::new();
        let data_len = (samples.len() * 4) as u32;
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for v in samples {
            out.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&path, out).unwrap();
    }

    /// Müzik açıkken ayar değiştirmek ve kapatmak beklemeden döner (eskiden iş parçacığı kendi
    /// kilidinde takılıyor, sayfa da onu bekleyip donuyordu).
    #[test]
    fn ayar_takilmaz() {
        let cfg = Config { react: React::Flash, source: Source::Drums, sensitivity: 1.5, brightness: 0.5, delay: Duration::ZERO };
        let m = Music::start(Tx::test(), cfg);
        std::thread::sleep(Duration::from_millis(300));
        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            m.set(Config { react: React::Colors, ..cfg });
            drop(m);
            let _ = done.send(());
        });
        assert!(wait.recv_timeout(Duration::from_secs(5)).is_ok(), "müzik iş parçacığı takıldı");
    }

    /// WAV okur (32 bit kayan nokta ya da 16 bit, mono ya da çok kanal): (örnek hızı, mono).
    fn wav(path: &str) -> (f32, Vec<f32>) {
        let d = std::fs::read(path).unwrap();
        let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        let (mut i, mut fmt, mut out) = (12, (1u16, 1u16, 48_000u32, 16u16), Vec::new());
        while i + 8 <= d.len() {
            let (id, len) = (&d[i..i + 4], u32_at(i + 4) as usize);
            if id == b"fmt " {
                fmt = (u16_at(i + 8), u16_at(i + 10), u32_at(i + 12), u16_at(i + 22));
            } else if id == b"data" {
                let (tag, ch, _, bits) = fmt;
                let step = bits as usize / 8;
                let body = &d[i + 8..(i + 8 + len).min(d.len())];
                for frame in body.chunks_exact(step * ch as usize) {
                    let s: f32 = frame
                        .chunks_exact(step)
                        .map(|b| if tag == 3 { f32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0 })
                        .sum();
                    out.push(s / ch as f32);
                }
            }
            i += 8 + len + (len & 1);
        }
        (fmt.2 as f32, out)
    }

    /// Bir sesi baştan sona bulucudan geçirir: (vuruşlar, ızgara vuruşları, son BPM).
    fn run(rate: f32, x: &[f32], threshold: f32) -> (Vec<Hit>, Vec<f64>, f32) {
        let mut det = Detector::new(rate);
        let (mut hits, mut beats) = (Vec::new(), Vec::new());
        let mut i = 0;
        while i + N <= x.len() {
            let now = (i + N) as f64 / rate as f64;
            if let Some(h) = det.frame(&x[i..i + N], now, threshold) {
                hits.push(h);
            }
            while let Some(b) = det.next_beat().filter(|b| *b <= now) {
                beats.push(b);
                det.advance_beat();
            }
            i += HOP;
        }
        (hits, beats, det.bpm())
    }

    /// Yapay davul: 120 BPM'de her vuruşta kick (80 Hz, kısa), aralarında zil (gürültü, tiz).
    /// Kick'ler ve ziller doğru türde bulunur, tempo ~120 çıkar, ızgara kick'lerin üstündedir.
    #[test]
    fn davul_ve_tempo() {
        let rate = 48_000.0;
        let beat = 0.5;
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let len = (20.0 * rate) as usize;
        let x: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / rate;
                let since = t % beat;
                let kick = if since < 0.12 { (2.0 * PI * 80.0 * since).sin() * (-since * 30.0).exp() * 0.8 } else { 0.0 };
                let hs = (t + beat / 2.0) % beat;
                let n = noise();
                let hat = if hs < 0.03 { n * (-hs * 150.0).exp() * 0.3 } else { 0.0 };
                kick + hat + 0.002 * n
            })
            .collect();
        let (hits, beats, bpm) = run(rate, &x, 3.0);
        let late = |h: &&Hit| h.at > 5.0;
        let kicks: Vec<_> = hits.iter().filter(late).filter(|h| h.kind == Kind::Kick).collect();
        let hats: Vec<_> = hits.iter().filter(late).filter(|h| h.kind == Kind::Hat).collect();
        assert!((bpm - 120.0).abs() < 4.0, "{bpm} BPM");
        // 5..20 sn: 30 kick, 30 zil.
        assert!(kicks.len() >= 27 && kicks.len() <= 31, "{} kick", kicks.len());
        assert!(hats.len() >= 24, "{} zil", hats.len());
        for k in &kicks {
            let off = k.at % 0.5;
            assert!(off.min(0.5 - off) < 0.04, "kick vuruştan uzak: {}", k.at);
        }
        let on = beats.iter().filter(|b| **b > 8.0).filter(|b| {
            let off = *b % 0.5;
            off.min(0.5 - off) < 0.05
        });
        let total = beats.iter().filter(|b| **b > 8.0).count();
        assert!(total > 0 && on.count() * 10 >= total * 9, "ızgara kick'lere oturmuyor");
    }

    /// Geliştirme aracı: kaydedilmiş şarkıyı çözümler, vuruşları `<wav>.olay.txt`'ye yazar.
    /// `COZUMLE=C:\\...\\kayit.wav cargo test -p lights cozumle -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn cozumle() {
        let path = std::env::var("COZUMLE").expect("COZUMLE=yol");
        let (rate, x) = wav(&path);
        let threshold: f32 = std::env::var("ESIK").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
        let (hits, beats, bpm) = run(rate, &x, threshold);
        let secs = x.len() as f32 / rate;
        let count = |k: Kind| hits.iter().filter(|h| h.kind == k).count();
        println!(
            "{secs:.0} sn, {bpm:.1} BPM, ızgara {} vuruş; kick {}, trampet {}, zil {} ({:.1}/sn)",
            beats.len(),
            count(Kind::Kick),
            count(Kind::Snare),
            count(Kind::Hat),
            hits.len() as f32 / secs
        );
        let mut out = String::new();
        for b in &beats {
            out += &format!("{b:.4} beat 0\n");
        }
        for h in &hits {
            out += &format!("{:.4} {:?} {:.2}\n", h.at, h.kind, h.strength);
        }
        std::fs::write(format!("{path}.olay.txt"), out).unwrap();
    }
}

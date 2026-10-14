//! Işıkla Bluetooth LE bağlantısı. Bütün Bluetooth işi tek bir iş parçacığında: sayfa ve müzik
//! komutları kuyruğa atar, iş parçacığı sırayla yazar. Kuyrukta aynı türden birden çok komut
//! birikirse (kaydırıcı sürüklenirken, müzikte) yalnızca sonuncusu gider. Işık yeni rengi
//! işlerken gelen paket rengi siler: renkten sonra `COLOR_SETTLE` beklenir, birlikte biriken
//! parlaklık renkten önce yazılır. Bağlanmadan gelen komutlar bağlanınca gider. Bağlantının
//! durumu değişince pencereye `WM_LINK` gelir.
//!
//! Bağlanmak: kayıtlı adres varsa önce doğrudan ona, olmazsa tarama (reklamdaki adı desteklenen
//! bir modelinki olan ilk cihaz). Bağlantı `GattSession::MaintainConnection` ile açık tutulur;
//! ışık uzaklaşıp dönerse Windows kendisi yeniden bağlanır.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Devices::Bluetooth::Advertisement::*;
use windows::Devices::Bluetooth::GenericAttributeProfile::*;
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::surucu::{self, Cmd, Model};

/// Bağlantının durumu değişti (bağlantı iş parçacığından ve Bluetooth olaylarından).
pub const WM_LINK: u32 = WM_APP + 90;

/// Tarama bu kadar sürer.
const SCAN: Duration = Duration::from_secs(10);

/// Renk komutundan sonra bir sonraki pakete kadar beklenen süre. STARLIGHT rengi işlerken (kamerayla
/// ölçüldü: 25 ms'de gelen parlaklık rengi siliyor, 30 ms'de silmiyor) gelen parlaklık komutu eski
/// rengi geri getiriyor; müzikte ışık tek renkte kalıyordu. Renk ardından renk sorun değil.
const COLOR_SETTLE: Duration = Duration::from_millis(60);

type Res<T> = Result<T, String>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum State {
    #[default]
    Off,
    Searching,
    Connecting,
    Connected,
}

#[derive(Clone, Default)]
pub struct Status {
    pub state: State,
    /// Bağlanılan (ya da bağlanılmaya çalışılan) ışığın adı.
    pub name: String,
    pub model: Option<&'static Model>,
    pub address: u64,
    /// Bağlıyken ışık ulaşılabilir mi (uzaklaşınca Windows yeniden dener).
    pub reachable: bool,
    /// Son bağlanma ya da yazma hatası.
    pub error: Option<String>,
}

enum Msg {
    Connect(Option<u64>),
    Disconnect,
    /// Komut ve kuyruğa atıldığı an (iz için).
    Send(Cmd, Instant),
    Quit,
}

/// Komut gönderebilen kopya (müzik iş parçacığı için).
#[derive(Clone)]
pub struct Tx(Sender<Msg>);

impl Tx {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.0.send(Msg::Send(cmd, Instant::now()));
    }

    /// Hiçbir yere gitmeyen kopya (testler).
    #[cfg(test)]
    pub fn test() -> Self {
        Tx(mpsc::channel().0)
    }
}

pub struct Link {
    tx: Sender<Msg>,
    status: Arc<Mutex<Status>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Link {
    pub fn start(hwnd: HWND) -> Self {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let (s, h) = (status.clone(), hwnd.0 as usize);
        let thread = std::thread::Builder::new().name("lights".into()).spawn(move || worker(rx, s, h)).ok();
        Self { tx, status, thread }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn connected(&self) -> bool {
        self.status.lock().unwrap().state == State::Connected
    }

    /// `address`: son bağlanılan ışık (yoksa taranır).
    pub fn connect(&self, address: Option<u64>) {
        let _ = self.tx.send(Msg::Connect(address));
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(Msg::Disconnect);
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(Msg::Send(cmd, Instant::now()));
    }

    pub fn tx(&self) -> Tx {
        Tx(self.tx.clone())
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn post(hwnd: usize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_LINK, WPARAM(0), LPARAM(0));
    }
}

/// Yazma izi: `lights\iz` dosyası varsa ışığa giden her paket `lights\iz.log`'a yazılır (aradaki
/// süre, kuyrukta bekleme, yazmanın sürdüğü, atlanan ve gidemeyen komutlar). Müzikte ışığın neyi
/// ne aralıkla aldığını görmek için; dosya yoksa hiçbir şey yapmaz.
struct Trace {
    file: Option<std::fs::File>,
    start: Instant,
    last: Option<Instant>,
}

impl Trace {
    fn open() -> Self {
        let dir = crate::dir();
        let mut file = if dir.join("iz").exists() { std::fs::File::create(dir.join("iz.log")).ok() } else { None };
        let start = Instant::now();
        if let Some(f) = file.as_mut() {
            ortak::log!("lights: yazma izi açık · {}", dir.join("iz.log").display());
            // Satırlardaki ms bu andan sayılır; duvar saati, kamera ve ses kayıtlarıyla eşleştirmek için.
            let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            use std::io::Write;
            let _ = writeln!(f, "# başlangıç unix_ms={:.1}", unix.as_secs_f64() * 1000.0);
        }
        Trace { file, start, last: None }
    }

    fn line(&mut self, at: Instant, text: &str) {
        use std::io::Write;
        let t = at.duration_since(self.start).as_secs_f64() * 1000.0;
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{t:>10.1}  {text}");
        }
    }

    fn note(&mut self, text: &str) {
        if self.file.is_some() {
            self.line(Instant::now(), text);
        }
    }

    /// `queued`: kuyruğa atıldığı an, `began`: yazmanın başladığı an.
    fn write(&mut self, cmd: Cmd, data: &[u8], queued: Instant, began: Instant, r: &Res<()>) {
        if self.file.is_none() {
            return;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let gap = self.last.map_or("      -".into(), |l| format!("+{:>6.1}", ms(began - l)));
        self.last = Some(began);
        let hex = data.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
        let end = match r {
            Ok(()) => String::new(),
            Err(e) => format!("  HATA {e}"),
        };
        let text = format!(
            "{gap}  kuyruk {:>5.1}  yazma {:>5.1}  {cmd:?}  [{hex}]{end}",
            ms(began - queued),
            ms(began.elapsed())
        );
        self.line(began, &text);
    }
}

fn worker(rx: Receiver<Msg>, status: Arc<Mutex<Status>>, hwnd: usize) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut trace = Trace::open();
    let set = |f: &dyn Fn(&mut Status)| {
        f(&mut status.lock().unwrap());
        post(hwnd);
    };
    let mut conn: Option<Conn> = None;
    // Komut toplarken gelen başka tür mesaj: sonraki turda işlenir.
    let mut next: Option<Msg> = None;
    // Yazılmayı bekleyen komutlar (her türden en son gelen) ve son rengin yazıldığı an.
    let mut pending: Vec<(Cmd, Instant)> = Vec::new();
    let mut color_at: Option<Instant> = None;
    loop {
        let msg = match next.take() {
            Some(m) => m,
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            },
        };
        match msg {
            Msg::Quit => break,
            Msg::Disconnect => {
                conn = None;
                pending.clear();
                set(&|s| {
                    s.state = State::Off;
                    s.error = None;
                });
            }
            Msg::Connect(address) => {
                conn = None;
                match connect(address, &status, hwnd) {
                    Ok(c) => {
                        let (name, model, address) = (c.name.clone(), c.model, c.address);
                        conn = Some(c);
                        set(&|s| {
                            *s = Status { state: State::Connected, name: name.clone(), model: Some(model), address, reachable: true, error: None }
                        });
                        ortak::log!("lights: bağlandı · {name} ({address:012X})");
                        trace.note(&format!("bağlandı · {name}"));
                        // Bağlanırken gelenler (müzik modunun başlangıç rengi gibi).
                        if let Some(c) = conn.as_mut()
                            && let Err((cmd, e)) = write_all(c, &mut pending, &mut color_at, &mut trace)
                        {
                            ortak::log!("lights: yazılamadı ({cmd:?}) · {e}");
                            set(&|s| s.error = Some(e.clone()));
                        }
                    }
                    Err(e) => {
                        ortak::log!("lights: bağlanılamadı · {e}");
                        set(&|s| {
                            s.state = State::Off;
                            s.error = Some(e.clone());
                        });
                    }
                }
            }
            Msg::Send(cmd, at) => {
                queue(&mut pending, cmd, at, &mut trace);
                drain(&rx, &mut pending, &mut next, &mut trace);
                let Some(c) = conn.as_mut() else {
                    trace.note(&format!("bağlı değil, bağlanınca gidecek {:?}", pending.last().map(|p| p.0)));
                    continue;
                };
                // Işık son rengi işliyorsa beklenir; bu arada gelenler de birleşir.
                let settle = color_at.map_or(Duration::ZERO, |t| COLOR_SETTLE.saturating_sub(t.elapsed()));
                if !settle.is_zero() && next.is_none() && pending.iter().any(|(x, _)| !matches!(x, Cmd::Color(_))) {
                    trace.note(&format!("renk oturuyor, {:.1} ms bekleniyor", settle.as_secs_f64() * 1000.0));
                    std::thread::sleep(settle);
                    drain(&rx, &mut pending, &mut next, &mut trace);
                }
                if let Err((cmd, e)) = write_all(c, &mut pending, &mut color_at, &mut trace) {
                    ortak::log!("lights: yazılamadı ({cmd:?}) · {e}");
                    set(&|s| s.error = Some(e.clone()));
                }
            }
        }
    }
    drop(conn);
}

/// Bekleyenlere ekler; aynı türden bekleyen varsa yerini alır.
fn queue(pending: &mut Vec<(Cmd, Instant)>, cmd: Cmd, at: Instant, trace: &mut Trace) {
    if let Some(i) = pending.iter().position(|(x, _)| x.same_kind(&cmd)) {
        let (old, _) = pending.remove(i);
        trace.note(&format!("atlandı {old:?} (yerine {cmd:?})"));
    }
    pending.push((cmd, at));
}

/// Kuyrukta biriken komutları bekleyenlere alır; başka tür mesaj gelirse onu `next`'e koyup durur.
fn drain(rx: &Receiver<Msg>, pending: &mut Vec<(Cmd, Instant)>, next: &mut Option<Msg>, trace: &mut Trace) {
    while next.is_none()
        && let Ok(m) = rx.try_recv()
    {
        match m {
            Msg::Send(c, at) => queue(pending, c, at, trace),
            other => *next = Some(other),
        }
    }
}

/// Bekleyenleri yazar; parlaklık renkten önce gider (ardından gelen parlaklık rengi siler).
/// Hata olursa kalanlar bırakılır.
fn write_all(c: &mut Conn, pending: &mut Vec<(Cmd, Instant)>, color_at: &mut Option<Instant>, trace: &mut Trace) -> Result<(), (Cmd, String)> {
    pending.sort_by_key(|(cmd, _)| !matches!(cmd, Cmd::Brightness(_)));
    for (cmd, at) in pending.drain(..) {
        let Some(data) = (c.model.encode)(cmd) else { continue };
        let began = Instant::now();
        let r = c.write(&data);
        trace.write(cmd, &data, at, began, &r);
        r.map_err(|e| (cmd, e))?;
        if matches!(cmd, Cmd::Color(_)) {
            *color_at = Some(Instant::now());
        }
    }
    Ok(())
}

// --- Bağlanmak ---

fn connect(address: Option<u64>, status: &Arc<Mutex<Status>>, hwnd: usize) -> Res<Conn> {
    let set = |state: State, name: &str| {
        let mut s = status.lock().unwrap();
        s.state = state;
        s.name = name.to_string();
        s.error = None;
        drop(s);
        post(hwnd);
    };
    if let Some(a) = address.filter(|a| *a != 0) {
        let name = status.lock().unwrap().name.clone();
        set(State::Connecting, &name);
        match Conn::open(a, status, hwnd) {
            Ok(c) => return Ok(c),
            Err(e) => ortak::log!("lights: kayıtlı ışığa bağlanılamadı ({a:012X}) · {e}; taranıyor"),
        }
    }
    set(State::Searching, "");
    let Some((a, name, _)) = scan(SCAN, |_, _| {})? else {
        return Err(t!(
            format!("No light found. Is it on and nearby? If its phone app is connected, close it. Supported: {}", surucu::names()),
            format!("Işık bulunamadı. Açık ve yakında mı? Telefon uygulaması bağlıysa kapat. Desteklenen: {}", surucu::names())
        ));
    };
    set(State::Connecting, &name);
    Conn::open(a, status, hwnd)
}

/// Reklam dinler: desteklenen ilk ışığı (adres, ad, model) döndürür, `timeout` içinde yoksa `None`.
/// `seen` her adlı reklamda çağrılır (komut satırındaki tarama listesi için).
fn scan(timeout: Duration, mut seen: impl FnMut(u64, &str)) -> Res<Option<(u64, String, &'static Model)>> {
    let err = |e: windows_core::Error| format!("Bluetooth: {}", e.message());
    let watcher = BluetoothLEAdvertisementWatcher::new().map_err(err)?;
    // Etkin tarama: ad bazı cihazlarda yalnızca tarama cevabında gelir.
    watcher.SetScanningMode(BluetoothLEScanningMode::Active).map_err(err)?;
    let (tx, rx) = mpsc::channel::<(u64, String)>();
    let handler = TypedEventHandler::<BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementReceivedEventArgs>::new(move |_, args| {
        if let Some(a) = args.as_ref() {
            let name = a.Advertisement()?.LocalName()?.to_string();
            if !name.is_empty() {
                let _ = tx.send((a.BluetoothAddress()?, name));
            }
        }
        Ok(())
    });
    let token = watcher.Received(&handler).map_err(err)?;
    watcher.Start().map_err(|e| no_bluetooth(&e))?;
    let end = Instant::now() + timeout;
    let mut found = None;
    while found.is_none() {
        let now = Instant::now();
        if now >= end {
            break;
        }
        match rx.recv_timeout((end - now).min(Duration::from_millis(250))) {
            Ok((a, name)) => {
                seen(a, &name);
                if let Some(m) = surucu::for_name(&name) {
                    found = Some((a, name, m));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if watcher.Status().map_err(err)? == BluetoothLEAdvertisementWatcherStatus::Aborted {
                    let _ = watcher.RemoveReceived(token);
                    return Err(t!("Bluetooth is off or missing", "Bluetooth kapalı ya da yok").into());
                }
            }
            Err(_) => break,
        }
    }
    let _ = watcher.Stop();
    let _ = watcher.RemoveReceived(token);
    Ok(found)
}

fn no_bluetooth(e: &windows_core::Error) -> String {
    t!(format!("Bluetooth is off or missing ({})", e.message()), format!("Bluetooth kapalı ya da yok ({})", e.message()))
}

fn gatt_error(s: GattCommunicationStatus) -> String {
    match s {
        GattCommunicationStatus::Unreachable => t!("the light is unreachable", "ışığa ulaşılamıyor").into(),
        GattCommunicationStatus::AccessDenied => t!("access denied", "erişim reddedildi").into(),
        GattCommunicationStatus::ProtocolError => t!("the light refused", "ışık reddetti").into(),
        s => format!("GATT {}", s.0),
    }
}

/// Açık bağlantı: yazılan özellik, kapatırken bırakılacaklar.
struct Conn {
    name: String,
    model: &'static Model,
    address: u64,
    device: BluetoothLEDevice,
    session: Option<GattSession>,
    service: GattDeviceService,
    ch: GattCharacteristic,
    /// Cevaplı yazma (yalnızca cevapsız desteklenmiyorsa).
    with_response: bool,
    without_ok: bool,
    status_token: i64,
    notify: Option<(GattCharacteristic, i64)>,
}

impl Conn {
    fn open(address: u64, status: &Arc<Mutex<Status>>, hwnd: usize) -> Res<Conn> {
        let err = |e: windows_core::Error| format!("Bluetooth: {}", e.message());
        let device = BluetoothLEDevice::FromBluetoothAddressAsync(address)
            .and_then(|op| op.join())
            .map_err(|_| t!("The light is not reachable", "Işığa ulaşılamıyor").to_string())?;
        let name = device.Name().map(|n| n.to_string()).unwrap_or_default();
        let saved = status.lock().unwrap().model;
        let model = surucu::for_name(&name).or(saved).ok_or_else(|| {
            t!(format!("{name} is not a supported light"), format!("{name} desteklenen bir ışık değil"))
        })?;
        let name = if name.is_empty() { model.name.to_string() } else { name };

        // Bağlantıyı açık tut: her komutta yeniden bağlanmasın, ışık dönünce kendiliğinden bağlansın.
        let session = device
            .BluetoothDeviceId()
            .and_then(|id| GattSession::FromDeviceIdAsync(&id))
            .and_then(|op| op.join())
            .and_then(|s| s.SetMaintainConnection(true).map(|_| s))
            .map_err(|e| ortak::log!("lights: oturum açılamadı · {}", e.message()))
            .ok();

        // Önce önbellekten (hızlı), bulunamazsa cihazın kendisinden.
        let mut found = None;
        for mode in [BluetoothCacheMode::Cached, BluetoothCacheMode::Uncached] {
            let r = device.GetGattServicesWithCacheModeAsync(mode).and_then(|op| op.join()).map_err(err)?;
            let st = r.Status().map_err(err)?;
            if st != GattCommunicationStatus::Success {
                if mode == BluetoothCacheMode::Uncached {
                    return Err(gatt_error(st));
                }
                continue;
            }
            let services = r.Services().map_err(err)?;
            for i in 0..services.Size().map_err(err)? {
                let s = services.GetAt(i).map_err(err)?;
                let Ok(cr) = s.GetCharacteristicsForUuidWithCacheModeAsync(model.write, mode).and_then(|op| op.join()) else { continue };
                if cr.Status().ok() == Some(GattCommunicationStatus::Success)
                    && let Ok(list) = cr.Characteristics()
                    && list.Size().unwrap_or(0) > 0
                    && let Ok(ch) = list.GetAt(0)
                {
                    found = Some((s, ch));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((service, ch)) = found else {
            return Err(t!("The light's control channel was not found", "Işığın komut kanalı bulunamadı").into());
        };
        let props = ch.CharacteristicProperties().map_err(err)?;
        let with = props.contains(GattCharacteristicProperties::Write);
        let without = props.contains(GattCharacteristicProperties::WriteWithoutResponse);

        let st = status_handler(&device, hwnd, status);
        let notify = model.notify.and_then(|u| subscribe(&service, u));

        let mut c = Conn {
            name,
            model,
            address,
            device,
            session,
            service,
            ch,
            // Cevapsız yazma tercih edilir: cevaplı her komut ~180 ms sürüyor, müzikte ışık
            // sesin gerisinde kalıyordu (cevapsız ~1 ms).
            with_response: with && !without,
            without_ok: without,
            status_token: st.unwrap_or(0),
            notify,
        };
        if !model.hello.is_empty() {
            c.write(model.hello)?;
        }
        Ok(c)
    }

    fn write(&mut self, data: &[u8]) -> Res<()> {
        let err = |e: windows_core::Error| format!("Bluetooth: {}", e.message());
        let send = |with: bool| -> Res<GattCommunicationStatus> {
            let w = DataWriter::new().map_err(err)?;
            w.WriteBytes(data).map_err(err)?;
            let buf = w.DetachBuffer().map_err(err)?;
            let opt = if with { GattWriteOption::WriteWithResponse } else { GattWriteOption::WriteWithoutResponse };
            self.ch.WriteValueWithOptionAsync(&buf, opt).and_then(|op| op.join()).map_err(err)
        };
        let st = send(self.with_response)?;
        if st == GattCommunicationStatus::Success {
            return Ok(());
        }
        // Cevaplı yazmayı reddeden ışıklar: cevapsız dene, tutarsa öyle devam et.
        if self.with_response && self.without_ok && send(false)? == GattCommunicationStatus::Success {
            self.with_response = false;
            return Ok(());
        }
        Err(gatt_error(st))
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some((ch, token)) = self.notify.take() {
            let _ = ch.RemoveValueChanged(token);
        }
        let _ = self.device.RemoveConnectionStatusChanged(self.status_token);
        let _ = self.service.Close();
        if let Some(s) = self.session.take() {
            let _ = s.Close();
        }
        let _ = self.device.Close();
        ortak::log!("lights: bağlantı kapandı · {}", self.name);
    }
}

/// Işık uzaklaşınca / dönünce durumdaki `reachable` değişir.
fn status_handler(device: &BluetoothLEDevice, hwnd: usize, status: &Arc<Mutex<Status>>) -> Option<i64> {
    // Olay iş parçacığı havuzundan gelir; durum paylaşılan kopyada.
    let status = status.clone();
    let handler = TypedEventHandler::<BluetoothLEDevice, windows_core::IInspectable>::new(move |dev, _| {
        if let Some(d) = dev.as_ref() {
            let up = d.ConnectionStatus()? == BluetoothConnectionStatus::Connected;
            let mut s = status.lock().unwrap();
            if s.state == State::Connected && s.reachable != up {
                s.reachable = up;
                drop(s);
                ortak::log!("lights: {}", if up { "ışık yeniden ulaşılabilir" } else { "ışığa ulaşılamıyor" });
                post(hwnd);
            }
        }
        Ok(())
    });
    device.ConnectionStatusChanged(&handler).ok()
}

/// Işığın cevaplarını loga yazar (protokolü anlamak için).
fn subscribe(service: &GattDeviceService, uuid: windows_core::GUID) -> Option<(GattCharacteristic, i64)> {
    let list = service.GetCharacteristicsForUuidAsync(uuid).ok()?.join().ok()?.Characteristics().ok()?;
    let ch = list.GetAt(0).ok()?;
    let handler = TypedEventHandler::<GattCharacteristic, GattValueChangedEventArgs>::new(|_, args| {
        if let Some(a) = args.as_ref() {
            let buf = a.CharacteristicValue()?;
            let mut v = vec![0u8; buf.Length()? as usize];
            DataReader::FromBuffer(&buf)?.ReadBytes(&mut v)?;
            ortak::log!("lights: ışık cevabı · {}", v.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
        }
        Ok(())
    });
    let token = ch.ValueChanged(&handler).ok()?;
    let ok = ch
        .WriteClientCharacteristicConfigurationDescriptorAsync(GattClientCharacteristicConfigurationDescriptorValue::Notify)
        .and_then(|op| op.join())
        .ok()
        == Some(GattCommunicationStatus::Success);
    if !ok {
        let _ = ch.RemoveValueChanged(token);
        return None;
    }
    Some((ch, token))
}

// --- Komut satırı ---

/// `winloom --lights-test`: her modelin örnek paketleri, sonra 10 saniyelik tarama (yakındaki
/// adlı Bluetooth cihazları, desteklenenler işaretli). Işık olmadan da çalışır.
pub fn cli_test() -> i32 {
    for m in surucu::MODELS {
        println!("{} paketleri:", m.name);
        for cmd in [Cmd::Power(true), Cmd::Color([255, 0, 0]), Cmd::Brightness(0.5), Cmd::Effect(1), Cmd::Speed(0.5), Cmd::Reverse(false)] {
            if let Some(p) = (m.encode)(cmd) {
                println!("  {cmd:?}: {}", p.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "));
            }
        }
    }
    println!("\n10 saniye taranıyor…");
    let mut names = std::collections::BTreeMap::new();
    let r = scan(SCAN, |a, n| {
        if names.insert(a, n.to_string()).is_none() {
            let mark = if surucu::for_name(n).is_some() { "  ← desteklenen ışık" } else { "" };
            println!("  {a:012X}  {n}{mark}");
        }
    });
    match r {
        Ok(Some((a, n, m))) => {
            println!("\nBulundu: {n} ({a:012X}, model {}). Bağlanılıyor…", m.id);
            let status = Arc::new(Mutex::new(Status { model: Some(m), ..Default::default() }));
            match Conn::open(a, &status, 0) {
                Ok(mut c) => {
                    println!("Bağlandı. Komut kanalı: {:?}, cevaplı yazma: {}", m.write, c.with_response);
                    // Işık gözle görülür bir sıra oynar: aç, kırmızı, yeşil, mavi, beyaz, efekt.
                    let steps = [
                        Cmd::Power(true),
                        Cmd::Brightness(1.0),
                        Cmd::Color([255, 0, 0]),
                        Cmd::Color([0, 255, 0]),
                        Cmd::Color([0, 0, 255]),
                        Cmd::Color([255, 255, 255]),
                        Cmd::Effect(1),
                    ];
                    let mut failed = 0;
                    for cmd in steps {
                        let Some(p) = (m.encode)(cmd) else { continue };
                        match c.write(&p) {
                            Ok(()) => println!("  {cmd:?}: gönderildi"),
                            Err(e) => {
                                println!("  {cmd:?}: {e}");
                                failed += 1;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(1200));
                    }
                    // Bir komutun ışığa varma süresi (müzikte gecikme bunun kadar).
                    let modes: &[bool] = if c.without_ok { &[true, false] } else { &[true] };
                    for &with in modes {
                        c.with_response = with;
                        let mut ms = Vec::new();
                        for i in 0..20u32 {
                            let Some(p) = (m.encode)(Cmd::Color(crate::music::hsv(i as f32 * 137.5, 1.0))) else { break };
                            let t = Instant::now();
                            if c.write(&p).is_ok() {
                                ms.push(t.elapsed().as_secs_f32() * 1000.0);
                            }
                            std::thread::sleep(Duration::from_millis(60));
                        }
                        ms.sort_by(f32::total_cmp);
                        if let (Some(lo), Some(hi)) = (ms.first(), ms.last()) {
                            let what = if with { "cevaplı" } else { "cevapsız" };
                            println!("  {what} yazma: orta {:.0} ms (en az {lo:.0}, en çok {hi:.0}), {} / 20", ms[ms.len() / 2], ms.len());
                        }
                    }
                    (failed > 0) as i32
                }
                Err(e) => {
                    println!("Bağlanılamadı: {e}");
                    1
                }
            }
        }
        Ok(None) => {
            println!("\nDesteklenen ışık bulunamadı ({} cihaz görüldü).", names.len());
            0
        }
        Err(e) => {
            println!("\n{e}");
            1
        }
    }
}

/// `winloom --lights-pace [cevapli]`: ışık müzikteki komut akışında rengi neden kaçırıyor. Her
/// adım loş beyazdan başlar ve ~4 saniye sürer; ekrana yalnızca adımın numarası ve ışığın hangi
/// renkte olması gerektiği yazılır. Yanlış renkte kalan adımlar hangi akışın sorun olduğunu söyler.
/// `cevapli`: her yazma ışığın cevabını bekler.
pub fn cli_pace(with_response: bool) -> i32 {
    println!("Işık aranıyor (winloom ve telefon uygulaması kapalı olmalı)…");
    let (a, name, m) = match scan(SCAN, |_, _| {}) {
        Ok(Some(found)) => found,
        Ok(None) => {
            println!("Desteklenen ışık bulunamadı.");
            return 1;
        }
        Err(e) => {
            println!("{e}");
            return 1;
        }
    };
    let status = Arc::new(Mutex::new(Status { model: Some(m), ..Default::default() }));
    let mut c = match Conn::open(a, &status, 0) {
        Ok(c) => c,
        Err(e) => {
            println!("Bağlanılamadı: {e}");
            return 1;
        }
    };
    if with_response {
        c.with_response = true;
    }
    println!("Bağlandı: {name}. Her adımda ışığın rengine bak; yanlış renkte kalan adımların numarasını not et.\n");
    let send = |c: &mut Conn, cmd: Cmd| {
        if let Some(p) = (m.encode)(cmd)
            && let Err(e) = c.write(&p)
        {
            println!("   (yazılamadı {cmd:?}: {e})");
        }
    };
    let wait = |ms: u64| std::thread::sleep(Duration::from_millis(ms));
    // Müzikteki gibi parlaklık nabızları: `every` ms'de bir tam / kısık, `ms` boyunca. `color`
    // verilirse her tam nabızdan önce renk yeniden gönderilir.
    let pulses = |c: &mut Conn, every: u64, ms: u64, color: Option<[u8; 3]>| {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut high = true;
        while Instant::now() < end {
            if let Some(col) = color.filter(|_| high) {
                send(c, Cmd::Color(col));
            }
            send(c, Cmd::Brightness(if high { 1.0 } else { 0.6 }));
            high = !high;
            wait(every);
        }
    };

    send(&mut c, Cmd::Power(true));
    wait(300);
    let steps: [(&str, [u8; 3], &str); 7] = [
        ("MAVİ", [0, 0, 255], "yalnızca renk"),
        ("YEŞİL", [0, 255, 0], "renk, hemen ardından tek parlaklık"),
        ("MOR", [140, 0, 255], "renk, ardından 140 ms'de bir nabız (müzikteki gibi)"),
        ("SARI", [255, 200, 0], "renk, 400 ms bekle, sonra 140 ms'de bir nabız"),
        ("TURKUAZ", [0, 220, 255], "renk, ardından 80 ms'de bir nabız (hızlı müzik)"),
        ("PEMBE", [255, 0, 140], "140 ms'de bir nabız, renk her nabızda yeniden"),
        ("TURUNCU", [255, 90, 0], "renk, ardından 300 ms'de bir nabız"),
    ];
    for (i, (what, color, how)) in steps.into_iter().enumerate() {
        // Her adım loş beyazdan başlar.
        send(&mut c, Cmd::Brightness(0.25));
        wait(400);
        send(&mut c, Cmd::Color([255, 255, 255]));
        wait(1500);
        println!("{}. {what} olmalı   ({how})", i + 1);
        send(&mut c, Cmd::Color(color));
        match i {
            0 => wait(4000),
            1 => {
                send(&mut c, Cmd::Brightness(1.0));
                wait(4000);
            }
            2 => pulses(&mut c, 140, 4000, None),
            3 => {
                wait(400);
                pulses(&mut c, 140, 3600, None);
            }
            4 => pulses(&mut c, 80, 4000, None),
            5 => pulses(&mut c, 140, 4000, Some(color)),
            _ => pulses(&mut c, 300, 4000, None),
        }
    }
    send(&mut c, Cmd::Brightness(0.25));
    println!("\nBitti. Yanlış renkte kalan (beyaz kalan ya da başka renk) adımların numaraları?");
    0
}

/// `winloom --lights-script <komutlar> <çıktı> [adres]`: ışığa satır satır komut gönderir (deney
/// için: kamerayla ölçen betik zamanları çıktıdaki duvar saatinden eşleştirir). Satırlar:
/// `color R G B`, `bright 0..1`, `power 0|1`, `wait ms`, `with 0|1` (cevaplı yazma), `mark ad`.
/// Çıktının her satırı: Unix zamanı (ms) ve yapılan iş. `adres` (onaltılık) verilirse taranmaz.
pub fn cli_script(input: &str, output: &str, address: Option<u64>) -> i32 {
    use std::io::Write;
    let Ok(text) = std::fs::read_to_string(input) else {
        println!("okunamadı: {input}");
        return 1;
    };
    let Ok(mut out) = std::fs::File::create(output) else {
        println!("yazılamadı: {output}");
        return 1;
    };
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64() * 1000.0;
    let mut say = |s: &str| {
        let _ = writeln!(out, "{:.1} {s}", now());
        let _ = out.flush();
    };
    let found = match address {
        Some(a) => surucu::MODELS.first().map(|m| (a, m.name.to_string(), *m)),
        None => match scan(SCAN, |_, _| {}) {
            Ok(f) => f,
            Err(e) => {
                say(&format!("hata {e}"));
                return 1;
            }
        },
    };
    let Some((a, _, m)) = found else {
        say("hata ışık bulunamadı");
        return 1;
    };
    let status = Arc::new(Mutex::new(Status { model: Some(m), ..Default::default() }));
    let mut c = match Conn::open(a, &status, 0) {
        Ok(c) => c,
        Err(e) => {
            say(&format!("hata bağlanılamadı {e}"));
            return 1;
        }
    };
    say("bağlandı");
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let num = |i: usize| w.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
        let cmd = match w.first().copied() {
            Some("color") => Cmd::Color([num(1) as u8, num(2) as u8, num(3) as u8]),
            Some("bright") => Cmd::Brightness(num(1)),
            Some("power") => Cmd::Power(num(1) != 0.0),
            Some("wait") => {
                std::thread::sleep(Duration::from_secs_f32(num(1) / 1000.0));
                continue;
            }
            Some("with") => {
                c.with_response = num(1) != 0.0;
                say(&format!("cevaplı {}", c.with_response));
                continue;
            }
            Some("mark") => {
                say(&format!("mark {}", w[1..].join(" ")));
                continue;
            }
            _ => continue,
        };
        let Some(p) = (m.encode)(cmd) else { continue };
        let t = Instant::now();
        match c.write(&p) {
            Ok(()) => say(&format!("{cmd:?} yazma {:.1} ms", t.elapsed().as_secs_f64() * 1000.0)),
            Err(e) => say(&format!("{cmd:?} HATA {e}")),
        }
    }
    say("bitti");
    0
}

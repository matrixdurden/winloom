//! winloom'ın penceresiyle konuşmak: araçlar ve ikinci kopyalar onu bu yolla öne getirir.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use std::sync::Mutex;

use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, SendMessageW, WM_APP, WM_COPYDATA,
};
use windows::core::{PCWSTR, w};

/// winloom'ın pencere sınıfı.
pub const CLASS: PCWSTR = w!("winloom");
/// WM_COPYDATA türü: "şu sekmeyi aç" isteği (boşsa yalnızca öne gel).
pub const COPY_TAB: usize = 0x6d74;
/// Bekleyen sekme isteği var (`take_tab`).
pub const WM_PENDING_TAB: u32 = WM_APP + 3;
/// Yeni sürüm kuruluyor: çalışan kopya tamamen kapansın (WM_CLOSE yalnızca gizler).
pub const WM_EXIT: u32 = WM_APP + 5;

static PENDING: Mutex<Option<String>> = Mutex::new(None);

/// Ekranı tutan bir araç açık (ekran görüntüsü seçimi): öbür araçlar ekran kenarı / köşe
/// hareketlerine tepki vermez (dock'un etkin köşeleri).
pub static SCREEN_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// winloom bir mesajı işlerken gelen sekme isteği: sonra uygulanmak üzere saklanır.
pub fn set_tab(tab: String) {
    *PENDING.lock().unwrap() = Some(tab);
}

pub fn take_tab() -> Option<String> {
    PENDING.lock().unwrap().take()
}

/// winloom'ın içinden (bir aracın sayfasından): winloom'ı o sekmede öne getirir. Mesaj olarak
/// gider; sayfa winloom'ın mesajını işlerken çağırabilir.
pub fn post_tab(tab: &str) {
    set_tab(tab.to_string());
    unsafe {
        if let Ok(h) = FindWindowW(CLASS, PCWSTR::null()) {
            let _ = PostMessageW(Some(h), WM_PENDING_TAB, WPARAM(0), LPARAM(0));
        }
    }
}

/// Çalışan winloom'a sekmeyi (araç kimliği, `tools`, `settings`) iletir ve onu öne getirir.
pub fn forward(tab: Option<&str>) {
    unsafe {
        let Ok(hwnd) = FindWindowW(CLASS, PCWSTR::null()) else { return };
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // Ön plana geçme hakkı bu süreçte (kullanıcı başlattı); öbürüne devret.
        let _ = AllowSetForegroundWindow(pid);
        let data = tab.unwrap_or("").as_bytes();
        let cds = COPYDATASTRUCT { dwData: COPY_TAB, cbData: data.len() as u32, lpData: data.as_ptr() as *mut _ };
        SendMessageW(hwnd, WM_COPYDATA, Some(WPARAM(0)), Some(LPARAM(&cds as *const _ as isize)));
    }
}

// --- Araçlar arası istek ---

/// Başka bir araçtan gelen istek: metin ve cevabın gideceği yer.
pub struct Request {
    pub body: String,
    reply: std::sync::mpsc::Sender<Result<String, String>>,
}

impl Request {
    pub fn reply(self, r: Result<String, String>) {
        let _ = self.reply.send(r);
    }
}

/// Bir aracın açtığı kapı: istekler kanala düşer, pencereye `msg` gönderilir (pencere yoksa
/// gönderilmez).
struct Port {
    name: &'static str,
    hwnd: usize,
    msg: u32,
    tx: std::sync::mpsc::Sender<Request>,
}

static PORTS: Mutex<Vec<Port>> = Mutex::new(Vec::new());

/// `name` adıyla istek kapısı açar (asistanın Işıklar'a "odayı yeşil yap" demesi gibi). İstek
/// gelince `hwnd`'ye `msg` gelir; araç kendi iş parçacığında kanaldan alıp `Request::reply` ile
/// cevaplar. Aynı adla yeniden açmak eskisinin yerini alır.
pub fn serve(name: &'static str, hwnd: HWND, msg: u32) -> std::sync::mpsc::Receiver<Request> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut ports = PORTS.lock().unwrap();
    ports.retain(|p| p.name != name);
    ports.push(Port { name, hwnd: hwnd.0 as usize, msg, tx });
    rx
}

pub fn close(name: &str) {
    PORTS.lock().unwrap().retain(|p| p.name != name);
}

/// `name` kapısına istek gönderip cevabı bekler. Kapının sahibinin iş parçacığından çağrılmamalı:
/// o iş parçacığı isteği işleyemez, süre dolar.
pub fn ask(name: &str, body: &str, timeout: std::time::Duration) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let ports = PORTS.lock().unwrap();
        let port = ports.iter().find(|p| p.name == name).ok_or_else(|| format!("{name} açık değil"))?;
        port.tx.send(Request { body: body.to_string(), reply: tx }).map_err(|_| format!("{name} kapandı"))?;
        if port.hwnd != 0 {
            unsafe {
                PostMessageW(Some(HWND(port.hwnd as *mut _)), port.msg, WPARAM(0), LPARAM(0)).map_err(|e| e.message())?;
            }
        }
    }
    rx.recv_timeout(timeout).map_err(|_| format!("{name} cevap vermedi"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn istek_kapisi() {
        assert!(ask("yok", "x", Duration::from_millis(50)).is_err());
        let rx = serve("deneme", HWND(std::ptr::null_mut()), 0);
        let t = std::thread::spawn(move || {
            let r = rx.recv().unwrap();
            let body = r.body.clone();
            r.reply(Ok(format!("aldım: {body}")));
        });
        assert_eq!(ask("deneme", "renk yeşil", Duration::from_secs(2)), Ok("aldım: renk yeşil".into()));
        t.join().unwrap();
        close("deneme");
        assert!(ask("deneme", "x", Duration::from_millis(50)).is_err());
    }
}

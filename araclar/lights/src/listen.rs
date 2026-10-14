//! Yalnızca müzik uygulamalarını dinlemek: varsayılan çıkıştaki ses oturumlarından çalan Spotify
//! ya da tarayıcı bulunur, yalnızca onun süreç ağacının sesi yakalanır (process loopback, Windows
//! 10 2004+ / 11). Discord, oyun ve bildirim sesleri ışığa yansımaz; tarayıcıda açık Discord ya
//! da video yansır.

use std::sync::mpsc;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Threading::{CreateEventW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, Interface, PWSTR, Ref, implement};

/// Dinlenen uygulamalar: exe adı (küçük harf, uzantısız) ve görünen adı. Sıra öncelik: Spotify
/// tarayıcıdan önce.
const APPS: [(&str, &str); 8] = [
    ("spotify", "Spotify"),
    ("chrome", "Chrome"),
    ("msedge", "Edge"),
    ("firefox", "Firefox"),
    ("brave", "Brave"),
    ("opera", "Opera"),
    ("vivaldi", "Vivaldi"),
    ("arc", "Arc"),
];

/// Yakalanan sesin biçimi: 48 kHz, 16 bit, stereo (Windows dönüştürür).
pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// Dinlenecek müzik uygulaması: süreç kimliği ve adı. Çalan (etkin oturumu olan) çalmayandan,
/// Spotify tarayıcıdan önce gelir; hiçbiri açık değilse `None`.
pub fn find(en: &IMMDeviceEnumerator) -> Option<(u32, &'static str)> {
    unsafe {
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None).ok()?;
        let list = mgr.GetSessionEnumerator().ok()?;
        let mut best: Option<(bool, usize, u32)> = None;
        for i in 0..list.GetCount().unwrap_or(0) {
            let Ok(ctl) = list.GetSession(i) else { continue };
            let Ok(state) = ctl.GetState() else { continue };
            if state == AudioSessionStateExpired {
                continue;
            }
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
            let pid = ctl2.GetProcessId().unwrap_or(0);
            let Some(rank) = exe_stem(pid).and_then(|s| APPS.iter().position(|(e, _)| *e == s)) else { continue };
            let active = state == AudioSessionStateActive;
            if best.is_none_or(|(a, r, _)| (active, std::cmp::Reverse(rank)) > (a, std::cmp::Reverse(r))) {
                best = Some((active, rank, pid));
            }
        }
        best.map(|(_, r, pid)| (pid, APPS[r].1))
    }
}

/// Sürecin exe adı, küçük harf ve uzantısız ("spotify").
fn exe_stem(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let r = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n);
        let _ = CloseHandle(p);
        r.ok()?;
        let path = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
        let file = path.rsplit('\\').next().unwrap_or(&path);
        Some(file.strip_suffix(".exe").unwrap_or(file).to_string())
    }
}

/// İstemcinin olay tutamacı: bırakılınca kapanır.
pub struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Etkinleştirme bitince haber verir (Windows başka iş parçacığından çağırır).
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Done(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(&self, _: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

impl IAgileObject_Impl for Done_Impl {}

/// Yalnızca `pid`'in süreç ağacının sesini veren, başlamaya hazır istemci ve onun olayı (istemci
/// durana dek tutulur). Biçim `RATE` / `CHANNELS`, 16 bit.
pub fn client(pid: u32) -> windows::core::Result<(IAudioClient, Event)> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // Parametreler PROPVARIANT içinde blob olarak verilir; bellek `params`'ın, bırakılmaz.
        let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                }),
            },
        });
        let (tx, rx) = mpsc::channel();
        let done: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*prop as *const PROPVARIANT), &done)?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| windows::core::Error::new(E_FAIL, "uygulama sesi açılamadı (zaman aşımı)"))?;
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        let client: IAudioClient = unk.ok_or_else(windows::core::Error::empty)?.cast()?;
        let block = CHANNELS * 2;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // Bu yakalama olay ister; olay beklenmez, paketler yine yoklanır.
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            200_000,
            0,
            &fmt,
            None,
        )?;
        let event = Event(CreateEventW(None, false, false, None)?);
        client.SetEventHandle(event.0)?;
        Ok((client, event))
    }
}

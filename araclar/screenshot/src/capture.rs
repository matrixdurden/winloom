//! Yakalama: bütün ekranlar bir kez kopyalanır (donmuş görüntü), üstünde tam ekran bir pencere
//! kararmış hâlini gösterir; seçilen alan aydınlık, ince çerçeveli; yazı yok. Seçim bırakılınca
//! yerinde kalır ve altında küçük bir araç çubuğu çıkar: kalem, kare ve renkler. Kenarından ya da
//! köşesinden (küçük karelerden) tutup sürüklemek boyutunu değiştirir. Araç seçili değilken seçimin
//! içinden tutup sürüklemek onu taşır; kalem ya da kare seçiliyken içeride çizilir. Ok tuşları da
//! taşır, Ctrl ile sağ ve alt kenarı oynatıp boyutlar (Shift ile 10 piksel). Çizimler görüntünün
//! üstünde yerinde kalır. Ctrl+C (ya da Enter) panoya kopyalar, Ctrl+S kaydetme yerini sorup PNG
//! olarak kaydeder (panoya da kopyalar), Ctrl+Z son çizimi geri alır, Esc vazgeçer; seçimin
//! dışında sürüklemek yeni seçim başlatır. Fare kıpırdadıkça yalnızca değişen bölge yeniden çizilir
//! (4K'da da akıcı). Açıkken öbür araçlar ekran köşelerine tepki vermez. Hepsi arayüz iş
//! parçacığında, GDI.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use ortak::util::wide;

const CLASS: PCWSTR = w!("winloom-ekran-goruntusu");
/// Karartma: donmuş görüntünün üstüne bu saydamlıkta siyah (0..255).
const DIM: u8 = 110;
/// Çizim renkleri (0xRRGGBB): kırmızı, sarı, yeşil, mavi, beyaz, siyah.
const COLORS: [u32; 6] = [0xff453a, 0xffd60a, 0x32d74b, 0x0a84ff, 0xffffff, 0x1c1c1e];
/// Çizginin kalınlığı (DIP).
const PEN_W: i32 = 3;
/// Seçimin boyutlamak için tutulan kenar şeridi, çerçevenin iki yanına (DIP); köşelerde iki katı.
const GRIP: i32 = 10;
/// Köşelerde ve kenar ortalarındaki tutamak karelerinin kenarı (DIP).
const KNOB: i32 = 7;
/// Araç çubuğu (DIP): düğme, aralık, kenar boşluğu, araçlarla renkler arası, seçimden uzaklık.
const BTN: i32 = 32;
const GAP: i32 = 2;
const PAD: i32 = 4;
const SEP: i32 = 10;
const OFF: i32 = 8;

thread_local! {
    /// Seçilen renk sonraki yakalamada da seçili gelir.
    static COLOR: Cell<usize> = const { Cell::new(0) };
}

/// COLORREF: 0x00BBGGRR.
fn colorref(c: u32) -> COLORREF {
    COLORREF(((c & 0xff) << 16) | (c & 0xff00) | (c >> 16))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tool {
    /// Seçili araç yok: seçimin içinden tutmak taşır.
    Move,
    Pen,
    Rect,
}

/// Araç çubuğunda bir yer.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Bar {
    Tool(Tool),
    Color(usize),
}

enum Shape {
    Pen(Vec<POINT>),
    /// Köşeleri (sürüklenen ve karşısı).
    Rect(POINT, POINT),
}

/// Seçimin tutulan kenarları (köşede ikisi).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Edges {
    l: bool,
    t: bool,
    r: bool,
    b: bool,
}

impl Edges {
    fn cursor(self) -> PCWSTR {
        match (self.l || self.r, self.t || self.b) {
            (true, true) if self.l == self.t => IDC_SIZENWSE,
            (true, true) => IDC_SIZENESW,
            (true, false) => IDC_SIZEWE,
            _ => IDC_SIZENS,
        }
    }
}

/// Çizilen bir şekil ve rengi (`COLORS` sırası).
struct Mark {
    shape: Shape,
    color: usize,
}

/// Donmuş görüntü ve seçim (koordinatlar pencerenin, yani sanal ekranın köşesine göre piksel).
struct Shot {
    hwnd: HWND,
    w: i32,
    h: i32,
    /// Donmuş görüntü, kararmış hâli ve çizim tamponu (her biri kendi DC'sinde seçili).
    src: (HDC, HBITMAP),
    dim: (HDC, HBITMAP),
    buf: (HDC, HBITMAP),
    /// Her rengin kalemi (ekran pikseli kalınlığında).
    pens: Vec<HPEN>,
    pen_w: i32,
    /// DIP → ekran pikseli.
    dpi: i32,
    /// Araç çubuğunun simge yazı tipi.
    icons: HFONT,
    tool: Tool,
    color: usize,
    /// İmlecin üstünde durduğu çubuk düğmesi.
    hover: Option<Bar>,
    /// Sürüklenen seçimin başladığı yer.
    anchor: Option<POINT>,
    sel: Option<RECT>,
    /// Taşınan seçim: tutulduğu yer ve o andaki seçim.
    moving: Option<(POINT, RECT)>,
    /// Boyutlanan seçim: tutulan kenarlar, tutulduğu yer ve o andaki seçim.
    resizing: Option<(Edges, POINT, RECT)>,
    /// Seçim bırakıldı: içinde çizilir, kısayollar çalışır.
    placed: bool,
    /// Çizimler (pencere pikseli); sonuncusu çizilmekte olabilir.
    marks: Vec<Mark>,
    drawing: bool,
    /// Kaydetme penceresinin ilk açtığı klasör.
    dir: PathBuf,
}

thread_local! {
    static SHOT: RefCell<Option<Shot>> = const { RefCell::new(None) };
}

/// Ekranı dondurur ve seçimi başlatır (zaten açıksa bir şey yapmaz).
pub fn start(dir: PathBuf) {
    if SHOT.with_borrow(|s| s.is_some()) {
        return;
    }
    unsafe {
        let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let screen = GetDC(None);
        let make = || {
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            SelectObject(dc, bmp.into());
            (dc, bmp)
        };
        let (src, dim, buf) = (make(), make(), make());
        let _ = BitBlt(src.0, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT);
        ReleaseDC(None, screen);
        // Kararmış kopya: donmuş görüntünün üstüne yarı saydam siyah.
        let _ = BitBlt(dim.0, 0, 0, w, h, Some(src.0), 0, 0, SRCCOPY);
        let black = CreateCompatibleDC(Some(dim.0));
        let one = CreateCompatibleBitmap(dim.0, 1, 1);
        SelectObject(black, one.into());
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: DIM, AlphaFormat: 0 };
        let _ = AlphaBlend(dim.0, 0, 0, w, h, black, 0, 0, 1, 1, blend);
        let _ = DeleteDC(black);
        let _ = DeleteObject(one.into());
        let _ = BitBlt(buf.0, 0, 0, w, h, Some(dim.0), 0, 0, SRCCOPY);

        let instance = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS,
            w!(""),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            for (dc, bmp) in [src, dim, buf] {
                let _ = DeleteDC(dc);
                let _ = DeleteObject(bmp.into());
            }
            return;
        };
        let dpi = GetDpiForWindow(hwnd).max(96);
        let pen_w = (PEN_W * dpi as i32 / 96).max(2);
        let pens = COLORS.iter().map(|&c| CreatePen(PS_SOLID, pen_w, colorref(c))).collect();
        let icons = CreateFontW(
            -(16 * dpi as i32 / 96),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe MDL2 Assets"),
        );
        SHOT.with_borrow_mut(|s| {
            *s = Some(Shot {
                hwnd,
                w,
                h,
                src,
                dim,
                buf,
                pens,
                pen_w,
                dpi: dpi as i32,
                icons,
                tool: Tool::Move,
                color: COLOR.get(),
                hover: None,
                anchor: None,
                sel: None,
                moving: None,
                resizing: None,
                placed: false,
                marks: Vec::new(),
                drawing: false,
                dir,
            })
        });
        ortak::hub::SCREEN_BUSY.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
}

fn norm(a: POINT, b: POINT) -> RECT {
    RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x) + 1, bottom: a.y.max(b.y) + 1 }
}

fn union(a: RECT, b: RECT) -> RECT {
    RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
}

fn inside(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

fn grow(r: RECT, d: i32) -> RECT {
    RECT { left: r.left - d, top: r.top - d, right: r.right + d, bottom: r.bottom + d }
}

impl Shot {
    fn px(&self, dip: i32) -> i32 {
        dip * self.dpi / 96
    }

    /// Seçimin, çerçevesinin ve tutamaklarının kapladığı yer.
    fn footprint(&self, r: RECT) -> RECT {
        grow(r, self.px(KNOB) / 2 + 2)
    }

    /// Tamponun `area` bölgesini baştan kurar: karartılmış zemin, aydınlık seçim, çizimler, çerçeve,
    /// araç çubuğu.
    fn compose(&self, area: RECT) {
        unsafe {
            let (l, t) = (area.left.max(0), area.top.max(0));
            let (r, b) = (area.right.min(self.w), area.bottom.min(self.h));
            if r <= l || b <= t {
                return;
            }
            let dc = self.buf.0;
            let _ = BitBlt(dc, l, t, r - l, b - t, Some(self.dim.0), l, t, SRCCOPY);
            let Some(s) = self.sel else { return };
            let _ = BitBlt(dc, s.left, s.top, s.right - s.left, s.bottom - s.top, Some(self.src.0), s.left, s.top, SRCCOPY);
            // Seçim taşınınca dışında kalan çizimler görünmez (görüntüde yerlerinde dururlar).
            IntersectClipRect(dc, s.left, s.top, s.right, s.bottom);
            self.draw_marks(dc, 0, 0);
            SelectClipRgn(dc, None);
            let white = CreateSolidBrush(COLORREF(0x00ededed));
            let _ = FrameRect(dc, &s, white);
            let _ = DeleteObject(white.into());
            if self.placed {
                self.paint_handles(dc, s);
            }
            self.paint_bar(dc);
        }
    }

    /// Köşelerde ve kenar ortalarında küçük kareler: buradan tutulup boyutlanır.
    fn paint_handles(&self, dc: HDC, s: RECT) {
        let k = self.px(KNOB) / 2;
        let (l, t, r, b) = (s.left, s.top, s.right - 1, s.bottom - 1);
        let (mx, my) = ((l + r) / 2, (t + b) / 2);
        unsafe {
            let fill = CreateSolidBrush(COLORREF(0x00ededed));
            let line = CreatePen(PS_SOLID, 1, COLORREF(0x00202020));
            let (ob, op) = (SelectObject(dc, fill.into()), SelectObject(dc, line.into()));
            for (x, y) in [(l, t), (mx, t), (r, t), (l, my), (r, my), (l, b), (mx, b), (r, b)] {
                let _ = Rectangle(dc, x - k, y - k, x + k + 1, y + k + 1);
            }
            SelectObject(dc, ob);
            SelectObject(dc, op);
            let _ = DeleteObject(fill.into());
            let _ = DeleteObject(line.into());
        }
    }

    /// Çizimleri `dc`'ye çizer; `(dx, dy)`: pencere pikselinden hedefe kaydırma.
    fn draw_marks(&self, dc: HDC, dx: i32, dy: i32) {
        unsafe {
            let old_pen = SelectObject(dc, self.pens[0].into());
            let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
            for m in &self.marks {
                SelectObject(dc, self.pens[m.color].into());
                match &m.shape {
                    Shape::Pen(st) => {
                        let Some(first) = st.first() else { continue };
                        let _ = MoveToEx(dc, first.x - dx, first.y - dy, None);
                        if st.len() == 1 {
                            let _ = LineTo(dc, first.x - dx + 1, first.y - dy);
                        }
                        for p in &st[1..] {
                            let _ = LineTo(dc, p.x - dx, p.y - dy);
                        }
                    }
                    Shape::Rect(a, b) => {
                        let r = norm(*a, *b);
                        let _ = Rectangle(dc, r.left - dx, r.top - dy, r.right - dx, r.bottom - dy);
                    }
                }
            }
            SelectObject(dc, old_brush);
            SelectObject(dc, old_pen);
        }
    }

    /// Bir şeklin kapladığı yer (kalem kalınlığı dahil).
    fn mark_area(&self, a: POINT, b: POINT) -> RECT {
        grow(norm(a, b), self.pen_w + 2)
    }

    fn redraw(&self, area: RECT) {
        self.compose(area);
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), Some(&area), false);
        }
    }

    /// Çizilmekte olan şekli `p`'ye uzatır ve yalnızca değişen bölgeyi yeniden çizdirir.
    fn extend(&mut self, p: POINT) {
        let Some(m) = self.marks.last_mut() else { return };
        let area = match &mut m.shape {
            Shape::Pen(st) => {
                let prev = *st.last().unwrap_or(&p);
                st.push(p);
                (prev, p)
            }
            Shape::Rect(a, b) => {
                let old = *b;
                *b = p;
                // Eski ve yeni kare: köşe `a` sabit, ikisinin birleşimi.
                let (a, o) = (*a, old);
                let r = union(norm(a, o), norm(a, p));
                (POINT { x: r.left, y: r.top }, POINT { x: r.right, y: r.bottom })
            }
        };
        self.redraw(self.mark_area(area.0, area.1));
    }

    /// Son çizimi geri alır.
    fn undo(&mut self) {
        if self.marks.pop().is_some()
            && let Some(s) = self.sel
        {
            self.redraw(self.footprint(s));
        }
    }

    fn set_sel(&mut self, sel: Option<RECT>) {
        let old = self.sel.map(|r| self.footprint(r));
        let old_bar = self.bar();
        self.sel = sel;
        let new = self.sel.map(|r| self.footprint(r));
        let new_bar = self.bar();
        let area = [old, new, old_bar.map(|b| b.0), new_bar.map(|b| b.0)].into_iter().flatten().reduce(union);
        if let Some(area) = area {
            self.redraw(area);
        }
    }

    /// `p` yerleşmiş seçimin hangi kenar şeridinde (köşede ikisi). Şerit çerçevenin dışına `GRIP`,
    /// içine en çok `GRIP` ama seçimin dörtte birinden fazla değil: küçük seçimin de ortası kalır.
    fn edge_at(&self, p: POINT) -> Option<Edges> {
        let r = self.sel.filter(|_| self.placed)?;
        let g = self.px(GRIP);
        if !inside(grow(r, g), p) {
            return None;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let near = |lo: i32, hi: i32, v: i32, d: i32| (v < lo + d, v >= hi - d);
        let (mut l, mut rt) = near(r.left, r.right, p.x, g.min(w / 4));
        let (mut t, mut b) = near(r.top, r.bottom, p.y, g.min(h / 4));
        // Köşeler daha geniş tutulur: bir kenardayken öbür yönde iki şerit.
        if t || b {
            (l, rt) = near(r.left, r.right, p.x, (2 * g).min(w / 3));
        }
        if l || rt {
            (t, b) = near(r.top, r.bottom, p.y, (2 * g).min(h / 3));
        }
        (l || t || rt || b).then_some(Edges { l, t, r: rt, b })
    }

    /// `p`'den tutmak seçimi taşır mı: araç seçili değilken içi (kenar şeridi boyutlar).
    fn grabs(&self, p: POINT) -> bool {
        self.tool == Tool::Move && self.placed && self.sel.is_some_and(|r| inside(r, p)) && self.edge_at(p).is_none()
    }

    /// Seçimi `(dx, dy)` kaydırır; ekranın dışına taşmaz.
    fn move_sel(&mut self, from: RECT, dx: i32, dy: i32) {
        let dx = dx.clamp(-from.left, self.w - from.right);
        let dy = dy.clamp(-from.top, self.h - from.bottom);
        self.set_sel(Some(RECT { left: from.left + dx, top: from.top + dy, right: from.right + dx, bottom: from.bottom + dy }));
    }

    /// Seçimin `e` kenarlarını `(dx, dy)` kaydırır; ekranın dışına taşmaz, karşı kenarı geçerse
    /// döner, en az bir piksel kalır.
    fn resize_sel(&mut self, from: RECT, e: Edges, dx: i32, dy: i32) {
        let mv = |v: i32, on: bool, d: i32, max: i32| if on { (v + d).clamp(0, max) } else { v };
        let span = |a: i32, b: i32, max: i32| match (a.min(b), a.max(b)) {
            (lo, hi) if hi > lo => (lo, hi),
            (lo, _) if lo < max => (lo, lo + 1),
            (lo, _) => (lo - 1, lo),
        };
        let (left, right) = span(mv(from.left, e.l, dx, self.w), mv(from.right, e.r, dx, self.w), self.w);
        let (top, bottom) = span(mv(from.top, e.t, dy, self.h), mv(from.bottom, e.b, dy, self.h), self.h);
        self.set_sel(Some(RECT { left, top, right, bottom }));
    }

    // --- Araç çubuğu ---

    /// Çubuk ve düğmeleri (yerleşmiş seçim varsa): seçimin altında ortalı; sığmazsa üstünde,
    /// o da sığmazsa içinde, altta.
    fn bar(&self) -> Option<(RECT, Vec<(Bar, RECT)>)> {
        let s = self.sel.filter(|_| self.placed)?;
        let (btn, gap, pad, sep, off) = (self.px(BTN), self.px(GAP), self.px(PAD), self.px(SEP), self.px(OFF));
        let w = 2 * pad + 8 * btn + 6 * gap + sep;
        let h = 2 * pad + btn;
        let mid = POINT { x: (s.left + s.right) / 2, y: (s.top + s.bottom) / 2 };
        let mon = monitor_at(mid, self.hwnd);
        let x = (mid.x - w / 2).clamp(mon.left + off, (mon.right - w - off).max(mon.left + off));
        let y = if s.bottom + off + h <= mon.bottom - off {
            s.bottom + off
        } else if s.top - off - h >= mon.top + off {
            s.top - off - h
        } else {
            s.bottom - off - h
        };
        let mut items = Vec::new();
        let mut l = x + pad;
        for (i, item) in [Bar::Tool(Tool::Pen), Bar::Tool(Tool::Rect)].into_iter().chain((0..COLORS.len()).map(Bar::Color)).enumerate() {
            if i == 2 {
                l += sep - gap;
            }
            items.push((item, RECT { left: l, top: y + pad, right: l + btn, bottom: y + pad + btn }));
            l += btn + gap;
        }
        Some((RECT { left: x, top: y, right: x + w, bottom: y + h }, items))
    }

    fn bar_at(&self, p: POINT) -> Option<Bar> {
        let (_, items) = self.bar()?;
        items.into_iter().find(|(_, r)| inside(*r, p)).map(|(b, _)| b)
    }

    fn on_bar(&self, p: POINT) -> bool {
        self.bar().is_some_and(|(r, _)| inside(r, p))
    }

    fn paint_bar(&self, dc: HDC) {
        let Some((bar, items)) = self.bar() else { return };
        let radius = self.px(10);
        let round = |r: RECT, fill: u32, line: Option<u32>, rad: i32| unsafe {
            let brush = CreateSolidBrush(colorref(fill));
            let pen = match line {
                Some(c) => CreatePen(PS_SOLID, self.px(1).max(1), colorref(c)),
                None => CreatePen(PS_NULL, 0, COLORREF(0)),
            };
            let (ob, op) = (SelectObject(dc, brush.into()), SelectObject(dc, pen.into()));
            let _ = RoundRect(dc, r.left, r.top, r.right, r.bottom, rad, rad);
            SelectObject(dc, ob);
            SelectObject(dc, op);
            let _ = DeleteObject(brush.into());
            let _ = DeleteObject(pen.into());
        };
        round(bar, 0x202020, Some(0x3a3a3a), radius);
        for (item, r) in items {
            let hover = self.hover == Some(item);
            match item {
                Bar::Tool(t) => {
                    if self.tool == t {
                        round(r, 0x454545, None, self.px(6));
                    } else if hover {
                        round(r, 0x2e2e2e, None, self.px(6));
                    }
                    // Segoe MDL2: kalem, boş kare.
                    let glyph = if t == Tool::Pen { "\u{E70F}" } else { "\u{E739}" };
                    let mut text: Vec<u16> = glyph.encode_utf16().collect();
                    let mut r = r;
                    unsafe {
                        let old = SelectObject(dc, self.icons.into());
                        SetBkMode(dc, TRANSPARENT);
                        SetTextColor(dc, colorref(0xededed));
                        DrawTextW(dc, &mut text, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                        SelectObject(dc, old);
                    }
                }
                Bar::Color(i) => {
                    if hover && self.color != i {
                        round(r, 0x2e2e2e, None, self.px(6));
                    }
                    // Seçili renk beyaz halkayla.
                    if self.color == i {
                        round(grow(r, -self.px(4)), 0x202020, Some(0xededed), self.px(6));
                    }
                    round(grow(r, -self.px(8)), COLORS[i], Some(0x5a5a5a), self.px(4));
                }
            }
        }
    }

    /// Çubuğu yeniden çizdirir (seçim, araç, renk, üstünde durulan düğme değişince).
    fn redraw_bar(&self) {
        if let Some((r, _)) = self.bar() {
            self.redraw(r);
        }
    }

    fn click_bar(&mut self, item: Bar) {
        match item {
            // Seçili araca yeniden basmak bırakır: içinden tutmak yine taşır.
            Bar::Tool(t) => self.tool = if self.tool == t { Tool::Move } else { t },
            Bar::Color(i) => {
                self.color = i;
                COLOR.set(i);
                // Renk seçmek çizmek içindir: araç yoksa kalem.
                if self.tool == Tool::Move {
                    self.tool = Tool::Pen;
                }
            }
        }
        self.redraw_bar();
    }

    /// Seçilen alanın pikselleri: 32 bit BGRA, yukarıdan aşağı.
    fn pixels(&self, r: RECT) -> (i32, i32, Vec<u8>) {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut out = vec![0u8; (w * h * 4) as usize];
        unsafe {
            let dc = CreateCompatibleDC(Some(self.src.0));
            let bmp = CreateCompatibleBitmap(self.src.0, w, h);
            SelectObject(dc, bmp.into());
            let _ = BitBlt(dc, 0, 0, w, h, Some(self.src.0), r.left, r.top, SRCCOPY);
            self.draw_marks(dc, r.left, r.top);
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            GetDIBits(dc, bmp, 0, h as u32, Some(out.as_mut_ptr().cast()), &mut bi, DIB_RGB_COLORS);
            let _ = DeleteDC(dc);
            let _ = DeleteObject(bmp.into());
        }
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        (w, h, out)
    }

    /// Seçileni panoya verir; `save`: klasöre de kaydeder.
    fn deliver(&self, r: RECT, save: bool) {
        let (w, h, px) = self.pixels(r);
        if let Err(e) = to_clipboard(self.hwnd, w, h, &px) {
            ortak::log!("ekran görüntüsü panoya konamadı: {e}");
            return;
        }
        let mut text = t!("Copied", "Kopyalandı").to_string();
        // Kaydetme yerini kullanıcı seçer (vazgeçerse yalnızca panoda kalır).
        if save && let Some(path) = ask_path(&self.dir) {
            match save_png(&path, w, h, &px) {
                Ok(()) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    text = t!(format!("Copied · saved as {name}"), format!("Kopyalandı · {name} olarak kaydedildi"));
                }
                Err(e) => ortak::log!("ekran görüntüsü kaydedilemedi: {e}"),
            }
        }
        ortak::osd::show("\u{E722}", 0xededed, &text);
    }
}

/// İmlecin olduğu ekranın tamamı (pencere koordinatlarında).
fn monitor_at(p: POINT, hwnd: HWND) -> RECT {
    unsafe {
        let mut screen = p;
        let _ = ClientToScreen(hwnd, &mut screen);
        let mon = MonitorFromPoint(screen, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let m = mi.rcMonitor;
        RECT { left: m.left - x, top: m.top - y, right: m.right - x, bottom: m.bottom - y }
    }
}

fn to_clipboard(hwnd: HWND, w: i32, h: i32, px: &[u8]) -> windows::core::Result<()> {
    unsafe {
        // CF_DIB: başlık + aşağıdan yukarıya satırlar.
        let header = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (w * h * 4) as u32,
            ..Default::default()
        };
        let size = size_of::<BITMAPINFOHEADER>() + px.len();
        let mem = GlobalAlloc(GMEM_MOVEABLE, size)?;
        let p = GlobalLock(mem) as *mut u8;
        std::ptr::copy_nonoverlapping(&header as *const _ as *const u8, p, size_of::<BITMAPINFOHEADER>());
        let rows = p.add(size_of::<BITMAPINFOHEADER>());
        let stride = (w * 4) as usize;
        for y in 0..h as usize {
            let src = &px[y * stride..(y + 1) * stride];
            std::ptr::copy_nonoverlapping(src.as_ptr(), rows.add((h as usize - 1 - y) * stride), stride);
        }
        let _ = GlobalUnlock(mem);
        OpenClipboard(Some(hwnd))?;
        let _ = EmptyClipboard();
        let r = SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(mem.0)));
        let _ = CloseClipboard();
        r.map(|_| ())
    }
}

/// Windows'un "Farklı kaydet" penceresi: adı tarih-saat, ilk açılışta `dir` (sonra Windows son
/// seçilen klasörü hatırlar). Vazgeçilirse `None`.
fn ask_path(dir: &std::path::Path) -> Option<PathBuf> {
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH};
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let name = format!("{}-{:02}-{:02} {:02}.{:02}.{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    let _ = std::fs::create_dir_all(dir);
    unsafe {
        let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let filter = [COMDLG_FILTERSPEC { pszName: w!("PNG"), pszSpec: w!("*.png") }];
        let _ = dlg.SetFileTypes(&filter);
        let _ = dlg.SetDefaultExtension(w!("png"));
        let name = wide(&name);
        let _ = dlg.SetFileName(PCWSTR(name.as_ptr()));
        let d = wide(&dir.display().to_string());
        if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(d.as_ptr()), None) {
            let _ = dlg.SetDefaultFolder(&item);
        }
        dlg.Show(None).ok()?;
        let path = dlg.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let out = PathBuf::from(path.to_string().ok()?);
        windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as *const _));
        Some(out)
    }
}

/// Pikselleri `path`'e PNG olarak yazar.
fn save_png(path: &std::path::Path, w: i32, h: i32, px: &[u8]) -> windows::core::Result<()> {
    let path = wide(&path.display().to_string());
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bmp = wic.CreateBitmapFromMemory(w as u32, h as u32, &GUID_WICPixelFormat32bppBGRA, w as u32 * 4, px)?;
        let stream = wic.CreateStream()?;
        stream.InitializeFromFilename(PCWSTR(path.as_ptr()), GENERIC_WRITE.0)?;
        let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(windows::core::Error::empty)?;
        frame.Initialize(None)?;
        frame.SetSize(w as u32, h as u32)?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut fmt)?;
        frame.WriteSource(&bmp, std::ptr::null())?;
        frame.Commit()?;
        enc.Commit()?;
    }
    Ok(())
}

/// Pencereyi kapatır ve donmuş görüntüyü bırakır; `deliver` verilirse önce o alan gönderilir
/// (`save`: klasöre de).
fn finish(deliver: Option<RECT>, save: bool) {
    let Some(shot) = SHOT.with_borrow_mut(Option::take) else { return };
    ortak::hub::SCREEN_BUSY.store(false, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        let _ = ShowWindow(shot.hwnd, SW_HIDE);
    }
    if let Some(r) = deliver.filter(|r| r.right > r.left && r.bottom > r.top) {
        shot.deliver(r, save);
    }
    unsafe {
        let _ = DestroyWindow(shot.hwnd);
        for (dc, bmp) in [shot.src, shot.dim, shot.buf] {
            let _ = DeleteDC(dc);
            let _ = DeleteObject(bmp.into());
        }
        for pen in shot.pens {
            let _ = DeleteObject(pen.into());
        }
        let _ = DeleteObject(shot.icons.into());
    }
}

fn point(lp: LPARAM) -> POINT {
    POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = unsafe { BeginPaint(hwnd, &mut ps) };
            SHOT.with_borrow(|s| {
                if let Some(s) = s {
                    let r = ps.rcPaint;
                    unsafe {
                        let _ = BitBlt(dc, r.left, r.top, r.right - r.left, r.bottom - r.top, Some(s.buf.0), r.left, r.top, SRCCOPY);
                    }
                }
            });
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        // Kenarda boyutlama oku, tutulunca seçimi taşıyan yerde dört yönlü ok, araç çubuğunda normal ok.
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            let mut p = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut p);
                let _ = ScreenToClient(hwnd, &mut p);
            }
            let cursor = SHOT.with_borrow(|s| match s {
                Some(s) if let Some((e, ..)) = s.resizing => e.cursor(),
                Some(s) if s.moving.is_some() => IDC_SIZEALL,
                Some(s) if s.on_bar(p) => IDC_ARROW,
                Some(s) if s.drawing || s.anchor.is_some() => IDC_CROSS,
                Some(s) if let Some(e) = s.edge_at(p) => e.cursor(),
                Some(s) if s.grabs(p) => IDC_SIZEALL,
                _ => IDC_CROSS,
            });
            unsafe {
                SetCursor(LoadCursorW(None, cursor).ok());
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let p = point(lp);
            SHOT.with_borrow_mut(|s| {
                let Some(s) = s else { return };
                // Araç çubuğu: düğmeye basılır, sürükleme yok.
                if s.on_bar(p) {
                    if let Some(item) = s.bar_at(p) {
                        s.click_bar(item);
                    }
                    return;
                }
                unsafe { SetCapture(hwnd) };
                match s.sel {
                    // Kenarından ya da köşesinden tutulur: seçim boyutlanır.
                    Some(r) if let Some(e) = s.edge_at(p) => s.resizing = Some((e, p, r)),
                    // Araç yoksa içinden tutulur: seçim taşınır.
                    Some(r) if s.grabs(p) => s.moving = Some((p, r)),
                    // Yerleşmiş seçimin içinde: şekil başlar.
                    Some(r) if s.placed && inside(r, p) => {
                        let shape = if s.tool == Tool::Rect { Shape::Rect(p, p) } else { Shape::Pen(vec![p]) };
                        s.marks.push(Mark { shape, color: s.color });
                        s.drawing = true;
                        s.extend(p);
                    }
                    // Dışında: yeni seçim (çizimler gider).
                    _ => {
                        let bar = s.bar();
                        s.marks.clear();
                        s.placed = false;
                        s.hover = None;
                        s.anchor = Some(p);
                        // Çubuk, tutamaklar ve çizimler gider.
                        let area = [bar.map(|b| b.0), s.sel.map(|r| s.footprint(r))].into_iter().flatten().reduce(union);
                        if let Some(area) = area {
                            s.redraw(area);
                        }
                    }
                }
            });
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            SHOT.with_borrow_mut(|s| {
                let Some(s) = s else { return };
                let p = point(lp);
                if let Some((e, from, r)) = s.resizing {
                    s.resize_sel(r, e, p.x - from.x, p.y - from.y);
                } else if let Some((from, r)) = s.moving {
                    s.move_sel(r, p.x - from.x, p.y - from.y);
                } else if s.drawing {
                    // Çizim seçimin dışına taşmaz.
                    if let Some(r) = s.sel {
                        s.extend(POINT { x: p.x.clamp(r.left, r.right - 1), y: p.y.clamp(r.top, r.bottom - 1) });
                    }
                } else if let Some(a) = s.anchor {
                    s.set_sel(Some(norm(a, p)));
                } else {
                    let hover = s.bar_at(p);
                    if hover != s.hover {
                        s.hover = hover;
                        s.redraw_bar();
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            unsafe {
                let _ = ReleaseCapture();
            }
            SHOT.with_borrow_mut(|s| {
                let Some(s) = s else { return };
                if std::mem::take(&mut s.drawing) || s.moving.take().is_some() || s.resizing.take().is_some() {
                    return;
                }
                if s.anchor.take().is_none() {
                    return;
                }
                // Sürüklemeden tıklamak: imlecin olduğu ekranın tamamı.
                let r = match s.sel {
                    Some(r) if r.right - r.left > 3 && r.bottom - r.top > 3 => r,
                    _ => monitor_at(point(lp), hwnd),
                };
                s.placed = true;
                s.set_sel(Some(r));
            });
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            finish(None, false);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
            let sel = SHOT.with_borrow(|s| s.as_ref().and_then(|s| s.sel.filter(|_| s.placed)));
            match VIRTUAL_KEY(wp.0 as u16) {
                VK_ESCAPE => finish(None, false),
                VK_RETURN if sel.is_some() => finish(sel, false),
                // Seçim yoksa Enter imlecin olduğu ekranı alır.
                VK_RETURN => {
                    let mut p = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut p);
                        let _ = ScreenToClient(hwnd, &mut p);
                    }
                    finish(Some(monitor_at(p, hwnd)), false);
                }
                VK_C if ctrl && sel.is_some() => finish(sel, false),
                VK_S if ctrl && sel.is_some() => finish(sel, true),
                // Ok tuşları seçimi kaydırır, Ctrl ile sağ ve alt kenarı oynatıp boyutlar (Shift ile
                // 10 piksel).
                k @ (VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN) if let Some(r) = sel => {
                    let n = if unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0 { 10 } else { 1 };
                    let (dx, dy) = match k {
                        VK_LEFT => (-n, 0),
                        VK_RIGHT => (n, 0),
                        VK_UP => (0, -n),
                        _ => (0, n),
                    };
                    SHOT.with_borrow_mut(|s| {
                        if let Some(s) = s {
                            if ctrl {
                                s.resize_sel(r, Edges { l: false, t: false, r: true, b: true }, dx, dy);
                            } else {
                                s.move_sel(r, dx, dy);
                            }
                        }
                    });
                }
                VK_Z if ctrl => SHOT.with_borrow_mut(|s| {
                    if let Some(s) = s {
                        s.undo();
                    }
                }),
                _ => {}
            }
            LRESULT(0)
        }
        // Başka bir pencere öne geçerse (Alt+Tab) yakalama biter.
        WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => {
            if SHOT.with_borrow(|s| s.as_ref().is_some_and(|s| s.hwnd == hwnd)) {
                finish(None, false);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

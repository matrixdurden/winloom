"""Müzikte ışık şarkıyla uyumlu mu: hoparlöre giden ses (loopback), kamera ve ışığa giden
komutlar (`lights\\iz.log`, `lights\\iz` dosyası varken) aynı anda kaydedilir ve karşılaştırılır.
winloom açık ve müzik modunda olmalı, şarkı çalmalı.

    py senkron.py [saniye]

Yazdıkları:
- şarkının temposu ve bas vuruşları (sesten, winloom'dan bağımsız),
- vuruştan ışığa komut gidene kadar geçen süre ve kaçan / fazla vuruşlar (ses ↔ iz),
- komuttan kamerada ışığın değişmesine kadar geçen süre (iz ↔ kamera),
- ışığın parlaklık oynaması (kamerada vuruşlar ne kadar fark ediliyor).
"""
import os
import sys
import threading
import time

import numpy as np
import soundcard as sc

import ortak

sys.stdout.reconfigure(encoding="utf-8")
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 40
SR = 48000
IZ = os.path.join(ortak.PROGRAMS, "lights", "iz.log")
os.makedirs(ortak.OUT, exist_ok=True)

cam = []  # (unix sn, b, g, r)
blocks = []  # (bloğun son örneğinin unix sn'si, örnekler)
stop = False


def watch():
    import cv2

    cap = ortak.camera()
    cap.set(cv2.CAP_PROP_FOURCC, cv2.VideoWriter_fourcc(*"MJPG"))
    cap.set(cv2.CAP_PROP_FPS, 30)
    while not stop:
        ok, f = cap.read()
        if ok:
            cam.append((time.time(), *ortak.roi_mean(f)))
    cap.release()


def listen():
    spk = sc.default_speaker()
    mic = sc.get_microphone(id=str(spk.name), include_loopback=True)
    with mic.recorder(samplerate=SR, blocksize=1024) as rec:
        while not stop:
            d = rec.record(numframes=1024)
            blocks.append((time.time(), d.mean(axis=1)))


threads = [threading.Thread(target=watch, daemon=True), threading.Thread(target=listen, daemon=True)]
for t in threads:
    t.start()
print(f"{secs:.0f} saniye kaydediliyor…")
time.sleep(secs + 1.5)
stop = True
for t in threads:
    t.join(3)

# --- Ses: bas vuruşları ---
audio = np.concatenate([b for _, b in blocks]).astype(np.float32)
# Örnek zamanı: ilk 20 bloğun son örnek zamanlarından en erken başlangıç (zamanlama gürültüsüne karşı).
starts = [t - sum(len(x) for _, x in blocks[: i + 1]) / SR for i, (t, _) in enumerate(blocks[:200])]
a0 = min(starts)
N, H = 2048, 256
win = np.hanning(N).astype(np.float32)
frames = np.lib.stride_tricks.sliding_window_view(audio, N)[::H] * win
spec = np.log1p(1000 * np.abs(np.fft.rfft(frames, axis=1)) / (N / 2))
freqs = np.fft.rfftfreq(N, 1 / SR)
ft = a0 + (np.arange(len(spec)) * H + N / 2) / SR  # karenin ortası


def flux(lo, hi):
    band = spec[:, (freqs >= lo) & (freqs < hi)]
    d = np.maximum(band[2:] - np.maximum.reduce([band[:-2], np.roll(band[:-2], 1, 1), np.roll(band[:-2], -1, 1)]), 0).mean(axis=1)
    return np.concatenate([[0, 0], d])


def peaks(x, t, gap):
    # Yerel tepe, son 3 saniyenin ortalamasının üstünde, `gap` sn'den sık değil.
    k = int(3 * SR / H)
    mean = np.convolve(x, np.ones(k) / k, mode="same")
    out = []
    for i in range(2, len(x) - 2):
        if x[i] >= x[i - 2 : i + 3].max() and x[i] > 1.8 * mean[i] + 1e-4 and (not out or t[i] - out[-1] > gap):
            out.append(t[i])
    return np.array(out)


bass = flux(30, 150)
mid = flux(150, 4000)
kicks = peaks(bass, ft, 0.12)
snares = peaks(mid, ft, 0.12)
fr = SR / H
env = bass - bass.mean()
lags = np.arange(int(fr * 60 / 200), int(fr * 60 / 60))
ac = np.array([np.dot(env[:-l], env[l:]) / (len(env) - l) for l in lags])
bpm = 60 * fr / lags[ac.argmax()]
print(f"\nSES  {len(audio) / SR:.1f} sn · tempo ~{bpm:.0f} BPM (yarısı/iki katı da olabilir) · bas vuruşu {len(kicks)}, orta {len(snares)}")
if len(kicks) == 0:
    print("ses yok mu? (şarkı çalmıyor ya da varsayılan çıkış başka)")
    sys.exit(1)

# --- İz: ışığa giden komutlar ---
cmds = []  # (unix sn, tür, değer)
rest = None
try:
    base = None
    for line in open(IZ, encoding="utf-8"):
        if line.startswith("# başlangıç unix_ms="):
            base = float(line.split("=")[1]) / 1000
            continue
        parts = line.split()
        if base is None or len(parts) < 2 or "yazma" not in line:
            continue
        t = base + float(parts[0]) / 1000
        if "Brightness(" in line:
            cmds.append((t, "B", float(line.split("Brightness(")[1].split(")")[0])))
        elif "Color(" in line:
            cmds.append((t, "C", line.split("Color(")[1].split(")")[0]))
except OSError:
    print("iz yok: lights\\iz dosyasını oluşturup winloom'ı yeniden başlat")
t0, t1 = ft[0], ft[-1]
cmds = [c for c in cmds if t0 <= c[0] <= t1]
bright = [c for c in cmds if c[1] == "B"]
if bright:
    vals = sorted({round(c[2], 2) for c in bright})
    rest = min(vals)
ups = np.array([c[0] for c in bright if c[2] > (rest or 0) + 0.01])
colors = [c for c in cmds if c[1] == "C"]
print(f"IZ   parlaklık komutu {len(bright)} (değerler {sorted({round(c[2], 2) for c in bright})}), parlama {len(ups)}, renk {len(colors)}")


def match(a, b, lo, hi):
    """`a`'nın her öğesi için `b`'de [lo, hi] sn sonra gelen ilki: (eşleşen gecikmeler, eşleşmeyen sayısı)."""
    d, miss = [], 0
    for x in a:
        y = b[(b >= x + lo) & (b <= x + hi)]
        if len(y):
            d.append(y[0] - x)
        else:
            miss += 1
    return np.array(d), miss


# Işık olayı: Flaş'ta parlama komutu, Renk'te renk komutu.
light = ups if len(ups) else np.array([c[0] for c in colors])
what = "parlama" if len(ups) else "renk değişimi"
if len(light):
    # Her olayın en yakın bas vuruşuna göre yeri: eksi, ışık komutu vuruştan önce gitti.
    off = np.array([(x - kicks[np.abs(kicks - x).argmin()]) for x in light])
    near = off[np.abs(off) < 0.25]
    print(f"     {what} komutu, en yakın bas vuruşuna göre: orta {np.median(near) * 1000:+.0f} ms (%10 {np.percentile(near, 10) * 1000:+.0f}, %90 {np.percentile(near, 90) * 1000:+.0f}); {len(near)}/{len(light)} vuruşa 250 ms'den yakın" if len(near) else f"     {what} hiçbir bas vuruşuna yakın değil")
    d, _ = match(kicks, light, -0.2, 0.35)
    print(f"     bas vuruşlarının {len(d)}/{len(kicks)} tanesinde ışık olayı var")
    gaps = np.diff(light)
    print(f"     olaylar arası: orta {np.median(gaps):.2f} sn, en uzun {gaps.max():.1f} sn")

# --- Kamera ---
if cam:
    c = np.array(cam)
    ct, lum = c[:, 0], 0.114 * c[:, 1] + 0.587 * c[:, 2] + 0.299 * c[:, 3]
    fps = len(ct) / (ct[-1] - ct[0])
    p10, p50, p90 = np.percentile(lum, [10, 50, 90])
    print(f"\nKAMERA {fps:.0f} kare/sn · parlaklık %10 {p10:.0f}, orta {p50:.0f}, %90 {p90:.0f} → oynama %{(p90 - p10) / max(p50, 1) * 100:.0f}")
    # Gecikme: olay dizisi (yumuşatılmış) ile kameradaki değişimin (renk ya da parlaklık) çapraz ilişkisi.
    grid = np.arange(ct[0], ct[-1], 0.005)
    rgb = np.stack([np.interp(grid, ct, c[:, k]) for k in (1, 2, 3)], axis=1)
    change = np.linalg.norm(np.gradient(rgb, axis=0), axis=1)

    def lag_to_camera(events, name):
        if len(events) < 5:
            return
        imp = np.zeros_like(grid)
        for e in events:
            i = int((e - grid[0]) / 0.005)
            if 0 <= i < len(imp):
                imp[i] = 1
        k = np.exp(-0.5 * (np.arange(-20, 21) / 6) ** 2)
        imp = np.convolve(imp, k, mode="same")
        best = max(range(0, 120), key=lambda s: np.dot(imp[: len(imp) - s], change[s:]))
        print(f"     {name} → kamerada değişim: ~{best * 5} ms")

    lag_to_camera(light, f"{what} komutu")
    lag_to_camera(kicks, "bas vuruşu (ses)")
np.savez(os.path.join(ortak.OUT, "senkron.npz"), kicks=kicks, snares=snares, ups=ups, light=light, cam=np.array(cam) if cam else np.zeros((0, 4)))
print(f"\nkayıt: {os.path.join(ortak.OUT, 'senkron.npz')}")

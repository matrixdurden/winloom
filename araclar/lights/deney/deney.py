"""Işık deneyi: kamera duvardaki yansımanın rengini sürekli ölçerken winloom ışığa betikteki
komutları gönderir (`winloom --lights-script`); sonra her `mark` bölümünün sonunda ışığın hangi
renkte kaldığı yazılır. Çalışan winloom kapatılır, sonunda yeniden açılır.

    py deney.py esik.txt

Betik satırları: color R G B · bright 0..1 · power 0|1 · wait ms · with 0|1 · mark ad. Kamera
ışığa ya da onun aydınlattığı duvara bakmalı; ölçüm alanı `ortak.ROI` (DENEY_ALAN ile değişir).
Bölümlerin son karesi %TEMP%\\winloom-deney\\<betik>\\ altına kaydedilir.
"""
import os
import subprocess
import sys
import threading
import time

import cv2
import numpy as np

import ortak

sys.stdout.reconfigure(encoding="utf-8")
script = os.path.abspath(sys.argv[1])
name = os.path.splitext(os.path.basename(script))[0]
out_dir = os.path.join(ortak.OUT, name)
os.makedirs(out_dir, exist_ok=True)

samples = []  # (unix ms, b, g, r)
frames = []  # (unix ms, kare), 250 ms'de bir
stop = False


def watch():
    cap = ortak.camera()
    keep = 0
    while not stop:
        ok, f = cap.read()
        t = time.time() * 1000
        if not ok:
            continue
        samples.append((t, *ortak.roi_mean(f)))
        if t - keep > 250:
            frames.append((t, f))
            keep = t
    cap.release()


was_running = ortak.stop_winloom()
if was_running:
    print("winloom kapatıldı, ışığın bağlantısı düşsün diye bekleniyor…")
    time.sleep(8)
th = threading.Thread(target=watch, daemon=True)
th.start()
time.sleep(2.5)
log = os.path.join(out_dir, "olaylar.txt")
args = [ortak.EXE, "--lights-script", script, log]
if a := ortak.address():
    args.append(a)
try:
    subprocess.run(args, timeout=600)
finally:
    time.sleep(0.8)
    stop = True
    th.join(3)
    if was_running:
        ortak.start_winloom()

events = []
for line in open(log, encoding="utf-8"):
    t, _, rest = line.strip().partition(" ")
    events.append((float(t), rest))
marks = [(t, e[5:]) for t, e in events if e.startswith("mark ")]
end = events[-1][0] if events else 0
print(f"kamera örnekleri: {len(samples)}, olaylar: {len(events)}")
for e in events:
    if "HATA" in e[1] or e[1].startswith("hata"):
        print("  ", e)

for i, (t, m) in enumerate(marks):
    t_end = marks[i + 1][0] if i + 1 < len(marks) else end + 600
    seg = [s for s in samples if t <= s[0] < t_end]
    if not seg:
        print(f"{m}: örnek yok")
        continue
    # Bölümün son 400 ms'i: ışığın vardığı renk.
    tail = [s for s in seg if s[0] >= t_end - 400] or seg[-3:]
    b, g, r = (sum(s[k] for s in tail) / len(tail) for k in (1, 2, 3))
    n, h, s_, v = ortak.label(b, g, r)
    # Bölüm boyunca görülen renklerin sırası (art arda aynılar birleşir).
    seq = []
    for smp in seg:
        ln = ortak.label(*smp[1:])[0]
        if not seq or seq[-1] != ln:
            seq.append(ln)
    # Geçiş hızı: bölümden önceki renkten vardığı renge yolun yarısı ve %90'ı ne zaman alındı
    # (bölümün ilk komutundan; kameranın kendi gecikmesi dahil).
    pre = [s for s in samples if t - 300 <= s[0] < t]
    first = next((e[0] for e in events if e[0] >= t and not e[1].startswith("mark")), None)
    speed = ""
    if pre and first:
        p = np.array([sum(s[k] for s in pre) / len(pre) for k in (1, 2, 3)])
        q = np.array([b, g, r])
        dist = np.linalg.norm(q - p)
        if dist > 8:
            prog = [(s[0], np.dot(np.array(s[1:]) - p, q - p) / dist**2) for s in seg]
            t50 = next((x for x, f in prog if f >= 0.5), None)
            t90 = next((x for x, f in prog if f >= 0.9), None)
            if t50 and t90:
                speed = f"  geçiş: yarı {t50 - first:4.0f} ms, %90 {t90 - first:4.0f} ms (fark {dist:.0f})"
    print(f"{m:<45} → {n:<12} (ton {h:3.0f} doy {s_:.2f}){speed}  akış: {' > '.join(seq)}")
    fr = [f for f in frames if t_end - 600 <= f[0] < t_end]
    if fr:
        cv2.imwrite(os.path.join(out_dir, f"{i:02d}.jpg"), cv2.resize(fr[-1][1], (320, 180)))

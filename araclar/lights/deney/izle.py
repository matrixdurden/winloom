"""Müzik çalarken ışığı izler (winloom açık kalır): kamera duvardaki rengi okur, hangi renkte ne
kadar kalındığını ve kaç kez kalıcı renk değiştiğini yazar. Işık tek renkte takılıyorsa burada
görünür; `lights\\iz` dosyası varsa `lights\\iz.log` ile zamanları karşılaştırılabilir.

    py izle.py [saniye]
"""
import collections
import os
import sys
import time

import cv2

import ortak

sys.stdout.reconfigure(encoding="utf-8")
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 30
os.makedirs(ortak.OUT, exist_ok=True)

cap = ortak.camera()
t0 = time.time()
while time.time() - t0 < 1.5:  # pozlama otursun
    cap.read()
seq = []
t0 = time.time()
while time.time() - t0 < secs:
    ok, f = cap.read()
    if ok:
        seq.append((time.time() - t0, ortak.label(*ortak.roi_mean(f))[0]))
        if len(seq) == 1:
            cv2.imwrite(os.path.join(ortak.OUT, "izle.jpg"), f)
cap.release()

count = collections.Counter(n for _, n in seq)
# Kısa titremeleri saymamak için en az 3 kare süren renk dilimleri.
runs = []
for t, n in seq:
    if runs and runs[-1][1] == n:
        runs[-1][2] += 1
    else:
        runs.append([t, n, 1])
stable = [r for r in runs if r[2] >= 3]
changes = sum(1 for a, b in zip(stable, stable[1:]) if a[1] != b[1])
print(f"{len(seq)} kare, {secs:.0f} sn")
print("renklerde geçen süre:", ", ".join(f"{n} %{c * 100 // max(len(seq), 1)}" for n, c in count.most_common()))
print("kalıcı renk değişimi:", changes)
print("dilimler:", " > ".join(f"{r[1]}@{r[0]:.1f}" for r in stable[:30]))

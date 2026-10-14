"""Kamera deneylerinin ortak parçaları: winloom'ın yeri, ışığın adresi, duvardaki ölçüm alanı ve
rengi ada çeviren sınıflandırma. Windows'taki Python'la çalışır (OpenCV gerekir: pip install
opencv-python)."""
import colorsys
import os
import subprocess
import tempfile

PROGRAMS = os.path.join(os.environ.get("LOCALAPPDATA", ""), "Programs", "winloom")
EXE = os.environ.get("WINLOOM_EXE", os.path.join(PROGRAMS, "winloom.exe"))
OUT = os.path.join(tempfile.gettempdir(), "winloom-deney")
# 640x360 karede duvarın LED'siz kısmı (x0, y0, x1, y1): yansıma rengi buradan okunur.
ROI = tuple(int(v) for v in os.environ.get("DENEY_ALAN", "200,40,400,320").split(","))


def address():
    """Işıklar'ın son bağlandığı ışığın adresi (yoksa winloom tarar)."""
    try:
        for line in open(os.path.join(PROGRAMS, "lights", "ayarlar.ini"), encoding="utf-8"):
            k, _, v = line.strip().partition("=")
            if k == "adres" and v not in ("", "0"):
                return v
    except OSError:
        pass
    return None


def stop_winloom():
    """Işık aynı anda tek bağlantı kabul eder: çalışan winloom kapanır (bağlantının düşmesi
    birkaç saniye sürer). Çalışıyor muydu."""
    r = subprocess.run(["taskkill", "/IM", "winloom.exe", "/F"], capture_output=True)
    return r.returncode == 0


def start_winloom():
    subprocess.Popen([os.path.join(PROGRAMS, "winloom.exe")], creationflags=subprocess.DETACHED_PROCESS)


def roi_mean(frame):
    x0, y0, x1, y1 = ROI
    b, g, r = frame[y0:y1, x0:x1].reshape(-1, 3).mean(axis=0)
    return b, g, r


def label(b, g, r):
    """Kameradaki ortalama rengin adı. Kamera beyazı morumsu, sarıyı turuncumsu görür."""
    h, s, v = colorsys.rgb_to_hsv(r / 255, g / 255, b / 255)
    h *= 360
    if s < 0.3 or (s < 0.55 and 260 <= h < 320):
        return "BEYAZ", h, s, v
    for lim, n in ((18, "KIRMIZI"), (45, "SARI/TURUNCU"), (75, "SARI"), (165, "YESIL"), (205, "TURKUAZ"), (255, "MAVI"), (295, "MOR"), (340, "PEMBE")):
        if h < lim:
            return n, h, s, v
    return "KIRMIZI", h, s, v


def camera():
    import cv2

    cap = cv2.VideoCapture(0, cv2.CAP_DSHOW)
    cap.set(cv2.CAP_PROP_FRAME_WIDTH, 640)
    cap.set(cv2.CAP_PROP_FRAME_HEIGHT, 360)
    return cap

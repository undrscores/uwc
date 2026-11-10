#!/usr/bin/env python3
"""Nested end-to-end check with real rendering and real pointer input.

Runs uwc's winit backend inside Xvfb with Mesa's software renderer, opens a layer-shell bar and two
windows with examples/paint, moves the X pointer with xdotool (winit delivers it as real input) and
reads what was drawn from screenshots. Covers what tests/headless.rs cannot: drawing, zoom, pinned
windows, fullscreen stacking, and pointer hit-testing (sloppy focus shows what is under the pointer).

Needs: Xvfb, xdotool, ImageMagick (import), python3-pil. Run from the repo root after
:   python3 tests/nested.py
"""
import socket, json, subprocess, time, os, glob, sys, tempfile
from PIL import Image
SP=tempfile.mkdtemp(prefix="uwc-nested-"); RD=SP+"/run"; os.makedirs(RD); os.chmod(RD,0o700)
DISPLAY=next(f":{n}" for n in range(90, 200) if not os.path.exists(f"/tmp/.X11-unix/X{n}"))
env=dict(os.environ, DISPLAY=DISPLAY, XDG_RUNTIME_DIR=RD, LIBGL_ALWAYS_SOFTWARE="1", RUST_LOG="error")
env.pop("WAYLAND_DISPLAY", None)
pids=[]
def spawn(args, **kw):
    p=subprocess.Popen(args, env=kw.get("env",env), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); pids.append(p); return p
open(SP+"/scen.kdl","w").write('options { border-width 0\n corner-radius 0\n shadow-size 0\n warp-cursor false\n root-color "#202020" }\n')
spawn(["Xvfb",DISPLAY,"-screen","0","1280x720x24"]); time.sleep(1.5)
spawn(["./target/debug/uwc","-c",SP+"/scen.kdl"]); time.sleep(3)
wenv=dict(env, WAYLAND_DISPLAY="wayland-1")
s=socket.socket(socket.AF_UNIX); s.connect(glob.glob(RD+"/uwc-*.sock")[0]); f=s.makefile()
def cmd(c): s.sendall(c.encode()+b"\n"); return json.loads(f.readline())
def win(t): return next(w for w in cmd("state")["windows"] if w["title"]==t)
spawn(["./target/debug/examples/paint","--bar","32","24283b"], env=wenv); time.sleep(0.6)
spawn(["./target/debug/examples/paint","A","400","300","d03030"], env=wenv); time.sleep(0.6)
spawn(["./target/debug/examples/paint","B","300","200","3050d0"], env=wenv); time.sleep(0.8)
A, B = win("A")["id"], win("B")["id"]
mon=cmd("state")["monitors"][0]; print("monitor", mon["width"], mon["height"], "work area", mon["work_area"])
cmd(f"move {A} 100 100"); cmd(f"move {B} 700 100"); cmd(f"pin {B}"); cmd("zoom 0.5 0 0")
time.sleep(0.5)
def px(x,y):
    subprocess.run(["import","-window","root",SP+"/scen.png"], env=env); return Image.open(SP+"/scen.png").convert("RGB").getpixel((x,y))
def color(p):
    r,g,b=p
    return "red" if r>g+80 and r>b+80 else "blue" if b>r+80 and b>g+50 else "bar" if (r,g,b)==(0x24,0x28,0x3b) else "black" if max(p)<10 else "root" if (r,g,b)==(32,32,32) else str(p)
ok=True
def check(name, got, want):
    global ok; good = got==want; ok&=good; print(("PASS" if good else "FAIL"), name, got, "" if good else f"(want {want})")
# zoom 0.5 from the origin: A (canvas 100,100 400x300) is drawn at 50,50 200x150; pinned B stays 700,100 300x200
check("zoomed A inside", color(px(240,190)), "red")
check("zoomed A is half size", color(px(260,210)), "root")
check("pinned B unzoomed (far corner)", color(px(990,290)), "blue")
def move(x,y):
    subprocess.run(["xdotool","mousemove",str(x),str(y)], env=env); time.sleep(0.25)
    st=cmd("state"); return st["cursor"], st["keyboard_focus"]
cur, kf = move(900,250); check("pointer reaches uwc", (cur["x"],cur["y"]), (900,250)); check("hover pinned B focuses it", kf, {"window": B})
cur, kf = move(150,120); check("hover zoomed A focuses it", kf, {"window": A})
cur, kf = move(260,210); check("empty canvas keeps focus", kf, {"window": A})
cur, kf = move(900,250); cur, kf = move(600,10); check("over the bar: focus stays", kf, {"window": B})
cmd(f"fullscreen {A}"); time.sleep(0.5)
check("fullscreen A covers the bar", color(px(5,10)), "red")
check("fullscreen-bg behind it", color(px(1000,600)), "black")
cur, kf = move(100,100); check("hover fullscreen A", kf, {"window": A})
cur, kf = move(900,250); check("pinned B hidden under fullscreen takes no pointer", kf, {"window": A})
cmd(f"unfullscreen {A}"); time.sleep(0.5)
check("bar back on top", color(px(600,10)), "bar")
# Sharp zoom: a client that re-renders at its fractional scale shows 1 px stripes at 1 screen px when
# zoomed 2x, once the zoom settled (stretched, they would be 2 px wide: half the transitions).
cmd(f"minimize {A}"); cmd("home")
spawn(["./target/debug/examples/paint","--stripes","S","300","200"], env=wenv); time.sleep(0.8)
S = win("S")["id"]; cmd(f"move {S} 50 50"); cmd("zoom 2 0 0")
time.sleep(0.8)  # zoom-settle-ms 200, then the client redraws
subprocess.run(["import","-window","root",SP+"/scen.png"], env=env)
row = [Image.open(SP+"/scen.png").convert("L").getpixel((x, 300)) for x in range(200, 240)]
flips = sum(1 for a, b in zip(row, row[1:]) if (a < 128) != (b < 128))
check("zoomed 2x stripes stay 1 px (sharp)", flips >= 35, True)
cmd("zoom 1 0 0")
# Per-window scale: S (canvas 50,50 300x200) at half size around its centre (200,150) covers
# 125..275 x 100..200 on screen, and the pointer hits it there and only there.
cmd(f"window-scale {S} 0.5"); time.sleep(0.4)
check("scaled window drawn inside its footprint", color(px(200,150)) != "root", True)
check("and not beyond it", color(px(290,190)), "root")
cur, kf = move(260,190); check("hover the scaled window", kf, {"window": S})
cur, kf = move(290,190); check("just outside it keeps focus", kf, {"window": S})
cmd(f"focus {B}"); cur, kf = move(290,190); check("outside the footprint does not focus it", kf, {"window": B})
for p in reversed(pids): p.kill()
print("ALL OK" if ok else "SOME FAILED")
sys.exit(0 if ok else 1)

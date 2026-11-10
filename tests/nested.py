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
# XDG_STATE_HOME: the session file must not touch the real one.
env=dict(os.environ, DISPLAY=DISPLAY, XDG_RUNTIME_DIR=RD, XDG_STATE_HOME=SP+"/state", LIBGL_ALWAYS_SOFTWARE="1", RUST_LOG="error")
env.pop("WAYLAND_DISPLAY", None)
pids=[]
import atexit
atexit.register(lambda: [p.kill() for p in reversed(pids)])  # also when a check crashes
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
# User shaders: a background and a border from GLSL files, a broken one falling back with the
# compiler's message over IPC, the bundled examples compiling, and an animated one animating.
open(SP+"/green.glsl","w").write("vec4 background(vec2 p, vec2 size, float time, vec2 view, float zoom) { return vec4(0.1, 0.8, 0.2, 1.0); }\n")
open(SP+"/magenta.glsl","w").write("vec4 border(vec2 p, vec2 size, float time, vec4 color, float focused) { return vec4(1.0, 0.0, 1.0, 1.0); }\n")
open(SP+"/broken.glsl","w").write("vec4 background(vec2 p, vec2 size, float time, vec2 view, float zoom) {\n  return vec4(oops);\n}\n")
cmd(f'set background-shader "{SP}/green.glsl"'); time.sleep(0.4)
r,g,b = px(1200,650); check("background shader fills empty canvas", (g > 150 and r < 80 and b < 100), True)
cmd("set border-width 8"); cmd("set corner-radius 0"); cmd(f'set border-shader "{SP}/magenta.glsl"'); time.sleep(0.4)
# S: footprint 125..275 x 100..200 at scale 0.5, so its border is 4 px outside: x 121..125.
r,g,b = px(123,150); check("border shader draws the ring", (r > 200 and g < 60 and b > 200), True)
cmd(f'set background-shader "{SP}/broken.glsl"'); time.sleep(0.4)
errs = cmd("state")["shader_errors"]
msg = errs.get(f"{SP}/broken.glsl", "")
check("broken shader reported with the compiler's message", "oops" in msg or "undeclared" in msg.lower(), True)
print("   (compiler said:", msg.strip()[:100] + ")")
check("broken shader falls back to root-color", color(px(1200,650)), "root")
for name in ["dots", "aurora"]:
    cmd(f'set background-shader "{os.getcwd()}/shaders/{name}.glsl"'); time.sleep(0.4)
    check(f"bundled {name}.glsl compiles", cmd("state")["shader_errors"], {})
cmd(f'set border-shader "{os.getcwd()}/shaders/neon.glsl"'); time.sleep(0.4)
check("bundled neon.glsl compiles", cmd("state")["shader_errors"], {})
a = px(123,150); time.sleep(0.6); b2 = px(123,150)
check("an animated border keeps moving", a != b2, True)
# Window previews (hyprland-toplevel-export, as Quickshell's ScreencopyView asks): the window alone,
# at its own size, without uwc's border, shadow or zoom.
def capture(args):
    r = subprocess.run(["./target/debug/examples/capture", *args, SP+"/cap.ppm"], env=wenv, capture_output=True, text=True, timeout=20)
    return r.returncode, r.stdout.strip()
rc, size = capture(["--title", "B"])
check("capture by toplevel handle", (rc, size), (0, "300x200"))
im = Image.open(SP+"/cap.ppm").convert("RGB")
check("the window's own pixels", im.getpixel((150,100)), (0x30,0x50,0xd0))
r,g,b = im.getpixel((2,2)); check("its own frame at the corner, not uwc's border", (r > 90 and b > 200 and g > 100), True)
check("the right way up: title strip at the top", (im.getpixel((150,20)), im.getpixel((150,190))[2] > 200), ((0xee,0xee,0xee), True))
rc, size = capture(["--id", str(B)])
check("capture by uwc window id", (rc, size), (0, "300x200"))
rc, _ = capture(["--id", "9999"])
check("an unknown window fails", rc, 2)
# A copy that waits for damage is answered when the window has new content, not at once: S redraws
# when its render scale changes, so a zoom releases it.
p2 = subprocess.Popen(["./target/debug/examples/capture", "--title", "S", SP+"/cap2.ppm", "--again"], env=wenv,
                      stdout=subprocess.PIPE, text=True); pids.append(p2)
time.sleep(1.0)
check("a second copy waits for new content", p2.poll(), None)
cmd("zoom 3 0 0"); 
try:
    out, _ = p2.communicate(timeout=5)
    check("and comes once the window redraws", "again after" in out, True)
except subprocess.TimeoutExpired:
    check("and comes once the window redraws", False, True)
cmd("zoom 1 0 0")
# Output capture (wlr-screencopy, what grim and Quickshell's monitor capture use).
st = cmd("state")["monitors"][0]
rc, size = capture(["--output"])
check("screencopy of the whole output", (rc, size), (0, f'{st["width"]}x{st["height"]}'))
im = Image.open(SP+"/cap.ppm").convert("RGB")
check("the bar where it is on screen", im.getpixel((600,10)), (0x24,0x28,0x3b))
r,g,b = im.getpixel((850,200)); check("pinned B where it is on screen", (r,g,b), (0x30,0x50,0xd0))
# A region exactly over B (pinned at 700,100, 300x200): B's own picture, top-left included, so the
# region is taken from the right place (not mirrored vertically).
rc, size = capture(["--region", "700", "100", "300", "200"])
check("screencopy of a region", (rc, size), (0, "300x200"))
im = Image.open(SP+"/cap.ppm").convert("RGB")
check("region centre is B", im.getpixel((150,100)), (0x30,0x50,0xd0))
r,g,b = im.getpixel((3,3)); check("region top-left is B's light frame", (r > 90 and b > 200 and g > 100), True)
check("region the right way up: B's title strip at its top", im.getpixel((150,20)), (0xee,0xee,0xee))
rc, _ = capture(["--region", "0", "0", "100", "60"])
im = Image.open(SP+"/cap.ppm").convert("RGB")
check("a region over the bar has the bar in its top rows", im.getpixel((50,5)) != im.getpixel((50,55)), True)
rc, _ = capture(["--region", "5000", "5000", "10", "10"])
check("a region off the output fails", rc, 2)
for p in reversed(pids): p.kill()
print("ALL OK" if ok else "SOME FAILED")
sys.exit(0 if ok else 1)

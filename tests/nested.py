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
open(SP+"/scen.kdl","w").write('tag 2 { root-color "#003366" }\noptions { mod-key "Alt"\n border-width 0\n corner-radius 0\n shadow-size 0\n warp-cursor false\n root-color "#202020" }\n')
spawn(["Xvfb",DISPLAY,"-screen","0","1280x720x24"]); time.sleep(1.5)
uwc_log=open(SP+"/uwc.log","w")
uwc_proc=subprocess.Popen(["./target/debug/uwc","-c",SP+"/scen.kdl"], env=env, stdout=uwc_log, stderr=uwc_log); pids.append(uwc_proc); time.sleep(3)
def _show_log():
    if uwc_proc.poll() is not None: print("uwc exited with", uwc_proc.returncode); print(open(SP+"/uwc.log").read()[-1500:])
atexit.register(_show_log)
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
# Eased view moves: IPC reports the target at once, the drawn view gets there over view-ms through the
# real frame loop. A (canvas 100,100) is drawn at 50,50 at zoom 0.5; after pan 200 it is at -50.
cmd("pan 200 0")
check("eased pan starts moving", cmd("state")["monitors"][0]["view_moving"], True)
time.sleep(0.8)
check("and settles", cmd("state")["monitors"][0]["view_moving"], False)
check("A drawn at its panned spot", (color(px(100,100)), color(px(240,190))), ("red", "root"))
cmd("pan -200 0"); time.sleep(0.8)
check("pan back: A where it was", color(px(240,190)), "red")
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
# The same through ext-image-copy-capture (what xdg-desktop-portal screen sharing uses): a window
# through the toplevel list, and the whole output.
rc, size = capture(["--ext-title", "B"])
check("ext capture of a window by toplevel handle", (rc, size), (0, "300x200"))
im = Image.open(SP+"/cap.ppm").convert("RGB")
check("ext window pixels", im.getpixel((150,100)), (0x30,0x50,0xd0))
check("ext window the right way up", im.getpixel((150,20)), (0xee,0xee,0xee))
rc, size = capture(["--ext-output"])
check("ext capture of the whole output", (rc, size), (0, f'{st["width"]}x{st["height"]}'))
im = Image.open(SP+"/cap.ppm").convert("RGB")
check("ext output: the bar where it is on screen", im.getpixel((600,10)), (0x24,0x28,0x3b))
check("ext output: pinned B where it is on screen", im.getpixel((850,200)), (0x30,0x50,0xd0))
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
# Window animations: a new window fades in over open-ms through the real frame loop (a slow one here so a
# screenshot can catch it), and ends up exactly its own colour; its reported position never moves.
cmd("set open-ms 3000"); cmd("set background-shader \"\"")
spawn(["./target/debug/examples/paint","F","300","200","d0d030"], env=wenv); time.sleep(0.7)
st=cmd("state"); fw=win("F"); F=fw["id"]; vp=st["monitors"][0]["viewport"]
fx_, fy_ = int((fw["x"]-vp["x"])*vp["zoom"]+fw["width"]*vp["zoom"]/2), int((fw["y"]-vp["y"])*vp["zoom"]+fw["height"]*vp["zoom"]/2)
mid = px(fx_, fy_)
check("a new window is still fading in", (st["monitors"][0]["animating"], mid != (0xd0,0xd0,0x30)), (True, True))
time.sleep(6)
check("fade finished: animating off", cmd("state")["monitors"][0]["animating"], False)
check("and the window is fully its colour", px(fx_, fy_), (0xd0,0xd0,0x30))
# The close animation: the window goes at once but its last frame stays, fading out over close-ms
# (slow here so a screenshot can catch it), shrunk about its centre, until the canvas is empty.
cmd("set close-ms 3000")
cmd(f"close {F}")
time.sleep(0.7)
st=cmd("state")
check("a closing window is gone from the state at once", any(w["id"]==F for w in st["windows"]), False)
check("but its last frame is still fading", (st["monitors"][0]["animating"], px(fx_, fy_) != (32,32,32)), (True, True))
time.sleep(6)
check("close fade finished: animating off", cmd("state")["monitors"][0]["animating"], False)
check("and the canvas is empty where it was", px(fx_, fy_), (32,32,32))
# Edge pan: Mod+dragging a window against the screen's edge carries the view along and the window
# stays under the pointer (so it can be taken further than one screen away).
cmd("set close-ms 0"); cmd("set open-ms 0"); cmd("set move-ms 0"); cmd("set view-ms 0"); cmd("zoom 1 0 0")
cmd(f"unfullscreen {A}")
spawn(["./target/debug/examples/paint","E","300","200","30d0d0"], env=wenv); time.sleep(0.8)
cmd(f"move {win('E')['id']} 100 150"); time.sleep(0.3)
# winit's X window gets the keyboard focus (there is no window manager to give it).
xwin = subprocess.run(["xdotool","search","--onlyvisible","--name",""], env=env, capture_output=True, text=True).stdout.split()[-1]
subprocess.run(["xdotool","windowfocus",xwin], env=env)
st=cmd("state"); vp0=st["monitors"][0]["viewport"]; a0=win("E")
ax, ay = int((a0["x"]-vp0["x"])*vp0["zoom"]+40), int((a0["y"]-vp0["y"])*vp0["zoom"]+40)
subprocess.run(["xdotool","keydown","Alt_L","mousemove",str(ax),str(ay),"mousedown","1","mousemove","900",str(ay),"mousemove","1279",str(ay)], env=env)
time.sleep(1.0)
subprocess.run(["xdotool","mouseup","1","keyup","Alt_L"], env=env); time.sleep(0.3)
st=cmd("state"); vp1=st["monitors"][0]["viewport"]; a1=win("E")
check("dragging a window to the edge pans the view", vp1["x"] > vp0["x"] + 200, True)
check("and the window went with it", a1["x"] - a0["x"] > 200 + (1279 - ax) - 60, True)
cmd("zoom 1 0 0")
# Hint mode (jump-hints, Mod+slash): the view zooms out and every window is labelled; every key goes to
# it; typing a label focuses that window and glides back to the zoom we came from; Escape puts it back.
cmd("set view-ms 150"); cmd("zoom 1 0 0")
for w_ in cmd("state")["windows"]:
    if w_["title"] not in ("E","A","B"): cmd(f"close {w_['id']}")
time.sleep(0.5)
cmd(f"restore {A}"); cmd(f"move {win('E')['id']} 100 150"); cmd(f"move {A} 1800 1200"); time.sleep(0.5)
z0 = cmd("state")["monitors"][0]["viewport"]["zoom"]
# Labels are badges in the focus colour: count them in a screenshot (borders off, a unique colour).
def count_color(rgb):
    subprocess.run(["import","-window","root",SP+"/scen.png"], env=env); im=Image.open(SP+"/scen.png").convert("RGB")
    return sum(1 for p_ in im.getdata() if p_==rgb)
cmd("set border-width 0"); cmd('set border-shader ""'); cmd('set focus-color "#00ff66"'); time.sleep(0.4)
none_before = count_color((0,255,102))
subprocess.run(["xdotool","keydown","Alt_L","key","slash","keyup","Alt_L"], env=env); time.sleep(0.6)
st = cmd("state")
check("hint labels are drawn as badges in the focus colour", (none_before, count_color((0,255,102)) > 600), (0, True))
check("hint mode is up and the view zoomed out to show every window", (st["hints"], st["monitors"][0]["viewport"]["zoom"] < z0), (True, True))
# Keys go to hint mode: 'q' is no label (ignored), and nothing reaches a client or a bind.
subprocess.run(["xdotool","key","q"], env=env); time.sleep(0.3)
check("a key that is no label is ignored", cmd("state")["hints"], True)
subprocess.run(["xdotool","key","Escape"], env=env); time.sleep(0.6)
st = cmd("state")
check("Escape puts the view back", (st["hints"], st["monitors"][0]["viewport"]["zoom"]), (False, z0))
subprocess.run(["xdotool","keydown","Alt_L","key","slash","keyup","Alt_L"], env=env); time.sleep(0.6)
focus_before = cmd("state")["keyboard_focus"]
subprocess.run(["xdotool","key","d"], env=env); time.sleep(0.8)
st = cmd("state")
check("typing a label jumps: hint mode is over", st["hints"], False)
check("and focuses one of the windows", st["keyboard_focus"] is not None, True)
time.sleep(0.5)
check("the view is back at the zoom it came from", cmd("state")["monitors"][0]["viewport"]["zoom"], z0)
# Per-tag looks: tag 2 has its own colour behind the canvas.
cmd("view 2"); time.sleep(0.5)
check("tag 2 has its own root colour", px(640, 400), (0,0x33,0x66))
cmd("view 1"); time.sleep(0.5)
check("and tag 1 has the global one again", px(1270, 700), (32,32,32))
# Background blur: a half-transparent black window (K) straddles the corner of a red one (W). Without
# blur the corner is sharp through it; with blur-radius it is smooth, and the right way up (the red
# is above the edge, so the pixel above the edge is the redder one). K's 10 px frame and its white
# strip are avoided, and W's own frame makes the red side lighter, so the checks are relative.
cmd("view 3"); time.sleep(0.4)
spawn(["./target/debug/examples/paint","W","400","300","d03030"], env=wenv); time.sleep(0.8)
spawn(["./target/debug/examples/paint","K","300","300","000000"], env=wenv); time.sleep(0.8)
vp = cmd("state")["monitors"][0]["viewport"]
kx, ky = 200+400-75, 150+300-120       # W is 400x300 at 200,150: its right edge is a quarter into K, its bottom 0.4
cmd(f"move {win('W')['id']} {vp['x']+200} {vp['y']+150}")
cmd(f"move {win('K')['id']} {vp['x']+kx} {vp['y']+ky}")
cmd('rule { match title="K"; opacity 0.5; corner-radius 0; shadow-size 0; border-width 0 }'); time.sleep(0.6)
ex, ey = kx+75, ky+120   # the corner of W, in screen px at zoom 1
def around():
    return (px(ex-8, ky+45)[0], px(ex+8, ky+45)[0], px(kx+30, ey-8)[0], px(kx+30, ey+8)[0])
l0, r0, t0, b0 = around()
check("without blur the edge is sharp (red side, root side; right edge)", (l0 > 100, r0 < 20), (True, True))
check("without blur the edge is sharp (red side, root side; bottom edge)", (t0 > 100, b0 < 20), (True, True))
cmd('rule { match title="K"; blur-radius 30 }'); time.sleep(0.8)
l, r_, t, b = around()
check("blur: the red side gets less red and the root side lighter (right edge)", (l < l0 - 10, r_ > r0 + 10), (True, True))
check("blur: and the red side is still the redder one", l > r_ + 6, True)
check("blur is the right way up (red above the edge, not below)", (t < t0 - 10, b > b0 + 10, t > b + 6), (True, True, True))
cmd('rule { match title="K"; blur-radius 0 }'); time.sleep(0.6)
check("blur-radius 0 turns it off again", around(), (l0, r0, t0, b0))
for t_ in ("W","K"): cmd(f"close {win(t_)['id']}")
cmd("view 1"); time.sleep(0.4)
# Dragging with real pointer input. A client that asks to be moved (what a CSD title bar does) is moved
# while the button is held and let go on release; dragging bare canvas, with a wallpaper layer covering
# it, pans the view. Both on tag 5, which is empty.
cmd("view 5"); time.sleep(0.4)
spawn(["./target/debug/examples/paint","--wallpaper","202040"], env=wenv); time.sleep(0.8)
spawn(["./target/debug/examples/paint","--mover","M","300","200","50d050"], env=wenv); time.sleep(0.8)
vp = cmd("state")["monitors"][0]["viewport"]
cmd(f"move {win('M')['id']} {vp['x']+400} {vp['y']+200}"); time.sleep(0.4)
m0 = win("M")
def drag(x0, y0, x1, y1):
    subprocess.run(["xdotool","mousemove",str(x0),str(y0),"mousedown","1"], env=env); time.sleep(0.3)
    for k in range(1, 5):
        subprocess.run(["xdotool","mousemove",str(x0+(x1-x0)*k//4),str(y0+(y1-y0)*k//4)], env=env); time.sleep(0.15)
    subprocess.run(["xdotool","mouseup","1"], env=env); time.sleep(0.4)
drag(550, 300, 630, 340)
m1 = win("M")
check("a client-requested move follows the pointer", (m1["x"]-m0["x"], m1["y"]-m0["y"]), (80, 40))
subprocess.run(["xdotool","mousemove","800","500"], env=env); time.sleep(0.4)
m2 = win("M")
check("and ends on release: the window stays put", (m2["x"], m2["y"]), (m1["x"], m1["y"]))
st = cmd("state"); vp0 = st["monitors"][0]["viewport"]
subprocess.run(["xdotool","mousemove","200","600"], env=env); time.sleep(0.3)
check("the wallpaper is under the pointer on empty canvas", cmd("state")["cursor"].get("under"), "layer:wallpaper")
drag(200, 600, 300, 600)
vp1 = cmd("state")["monitors"][0]["viewport"]
check("dragging empty canvas over a wallpaper pans the view", round(vp0["x"] - vp1["x"]), 100)
cmd("view 1"); time.sleep(0.4)
# Popup grabs: a menu (a grabbed popup) closes when a click lands outside it, and that click is not
# also a drag or a focus change; a click on the popup itself keeps it open.
p3 = subprocess.Popen(["./target/debug/examples/paint","--popup"], env=wenv,
                      stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True); pids.append(p3)
time.sleep(1.5)
check("the menu window is up", any(w["title"]=="P" for w in cmd("state")["windows"]), True)
pw = win("P"); vp = cmd("state")["monitors"][0]["viewport"]
# The popup sits at the window's top-left; its centre and a far corner of the canvas.
(pcx, pcy) = (int((pw["x"]-vp["x"])*vp["zoom"]+50), int((pw["y"]-vp["y"])*vp["zoom"]+25))
subprocess.run(["xdotool","mousemove",str(pcx),str(pcy),"click","1"], env=env); time.sleep(0.5)
check("a click on the popup does not close it", p3.poll(), None)
subprocess.run(["xdotool","mousemove","900","400","click","1"], env=env); time.sleep(0.5)
try:
    out, _ = p3.communicate(timeout=5)
    check("a click outside closes the menu", "popup done" in out, True)
except subprocess.TimeoutExpired:
    check("a click outside closes the menu", False, True)
for p in reversed(pids): p.kill()
print("ALL OK" if ok else "SOME FAILED")
sys.exit(0 if ok else 1)

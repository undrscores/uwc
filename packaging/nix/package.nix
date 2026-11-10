{ lib
, rustPlatform
, pkg-config
, makeWrapper
, patchelf
, libxkbcommon
, libinput
, libgbm
, libdrm
, seatd
, systemd # libudev
, wayland
, libglvnd
, xwayland
}:

rustPlatform.buildRustPackage {
  pname = "uwc";
  version = "0.1.0";

  src = lib.cleanSource ../..;
  cargoLock.lockFile = ../../Cargo.lock;

  nativeBuildInputs = [ pkg-config makeWrapper patchelf ];
  buildInputs = [ libxkbcommon libinput libgbm libdrm seatd systemd wayland libglvnd ];

  # The tests start compositors and wayland clients; run them with `cargo test` in the dev shell.
  doCheck = false;

  postInstall = ''
    mkdir -p $out/share/wayland-sessions $out/share/xdg-desktop-portal $out/share/uwc
    substitute uwc-session $out/bin/uwc-session --replace-fail @PREFIX@ $out
    chmod 755 $out/bin/uwc-session
    substitute uwc.desktop $out/share/wayland-sessions/uwc.desktop --replace-fail @PREFIX@ $out
    install -m 644 uwc-portals.conf $out/share/xdg-desktop-portal/uwc-portals.conf
    install -m 644 config.def.kdl $out/share/uwc/config.def.kdl
    cp -r shaders $out/share/uwc/shaders
  '';

  # EGL and GLES are opened with dlopen, so they have to be findable at run time; Xwayland is
  # started by name when `xwayland true` is set.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath [ libglvnd wayland libxkbcommon ]} $out/bin/uwc
    wrapProgram $out/bin/uwc --suffix PATH : ${lib.makeBinPath [ xwayland ]}
  '';

  passthru.providedSessions = [ "uwc" ];

  meta = {
    description = "Wayland compositor with an infinite 2D canvas and dwm-style tags";
    homepage = "https://github.com/undrscores/uwc";
    license = lib.licenses.mit;
    mainProgram = "uwc";
    platforms = lib.platforms.linux;
  };
}

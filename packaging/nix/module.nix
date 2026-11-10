# NixOS module: `programs.uwc.enable = true;` adds uwc to the display manager's session list and
# sets up what a session needs (graphics drivers, portals for screen sharing).
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.uwc;
in
{
  options.programs.uwc = {
    enable = lib.mkEnableOption "uwc, an infinite-canvas Wayland compositor";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.uwc;
      defaultText = lib.literalExpression "uwc.packages.\${system}.uwc";
      description = "The uwc package to use.";
    };

    portals = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Configure xdg-desktop-portal for the uwc session: screen sharing and screenshots through
        xdg-desktop-portal-wlr, everything else through the GTK portal.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    # Shows up in the display manager (`providedSessions` in the package); `uwc-session` also
    # works from a TTY.
    services.displayManager.sessionPackages = [ cfg.package ];
    hardware.graphics.enable = lib.mkDefault true;
    # Seat access comes from logind (the default); for seatd instead, enable services.seatd.

    xdg.portal = lib.mkIf cfg.portals {
      enable = true;
      wlr.enable = true;
      extraPortals = [ pkgs.xdg-desktop-portal-gtk ];
      config.uwc = {
        default = [ "gtk" ];
        "org.freedesktop.impl.portal.ScreenCast" = [ "wlr" ];
        "org.freedesktop.impl.portal.Screenshot" = [ "wlr" ];
      };
    };
  };
}

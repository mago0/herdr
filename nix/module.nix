# Home Manager module: Home Manager's `programs.herdr`, with this fork as its package.
self:
{
  config,
  lib,
  pkgs,
  ...
}:

{
  config = lib.mkIf config.programs.herdr.enable {
    programs.herdr.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  };
}

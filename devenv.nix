# Drive's dev environment: devenv-fleet's Rust module (the 1.99.1 pin, mold,
# kache as the wrapper) is all the workspace needs; dx 0.8 for the web apps
# comes from the prototype's tools (see README).
{ pkgs, ... }:
{
  packages = [ pkgs.pkg-config pkgs.fuse3 pkgs.openssl ];
}

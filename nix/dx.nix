# dx (dioxus-cli) at the version of the dioxus crate in Cargo.lock: dx
# writes the asset!() table into the binary it builds, and only the
# matching version can. nixpkgs' recipe, with its source moved to the
# crates.io release.
{
  dioxus-cli,
  fetchCrate,
  rustPlatform,
}:
dioxus-cli.overrideAttrs (
  final: _: {
    version = "0.8.0-alpha.3";
    src = fetchCrate {
      pname = "dioxus-cli";
      inherit (final) version;
      hash = "sha256-n1Lk9eCjr1vGqvCDbrOmbU7TTvdcGdx/chRsj4Z3a+Q=";
    };
    cargoDeps = rustPlatform.fetchCargoVendor {
      inherit (final) src;
      name = "dioxus-cli-${final.version}-vendor";
      hash = "sha256-nqGrW0Sf7DWdrSXSKcSl+FlO+1NY4uZA7G7G4AGItuw=";
    };
    doCheck = false;
    doInstallCheck = false;
  }
)

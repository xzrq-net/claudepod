{
  pkgs,
  craneLib,
  username,
  image,
}:
craneLib.buildPackage {
  src = craneLib.cleanCargoSource ./.;
  cargoExtraArgs = "--bin claudepod-start";

  CLAUDEPOD_USERNAME = username;
  CLAUDEPOD_IMAGE = "${image}";
  CLAUDEPOD_PODMAN = "${pkgs.podman}/bin/podman";

  postInstall = ''
    mv "$out/bin/claudepod-start" "$out/bin/claudepod"
    ln -s claudepod "$out/bin/gptpod"
  '';
}

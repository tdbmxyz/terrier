# NixOS module for terrier, the immobilier price tracker. Imported from the flake as
# `nixosModules.terrier`; `self` provides the default packages.
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.terrier;
  settingsFormat = pkgs.formats.toml {};
  configFile = settingsFormat.generate "terrier.toml" cfg.settings;
in {
  options.services.terrier = {
    enable = lib.mkEnableOption "terrier, the self-hosted deal tracker";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.terrier-server;
      defaultText = lib.literalExpression "terrier.packages.\${system}.terrier-server";
      description = "terrier-server package to run.";
    };

    webPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.terrier-web;
      defaultText = lib.literalExpression "terrier.packages.\${system}.terrier-web";
      description = "Built web frontend served by the server (null to disable).";
    };

    address = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0";
      description = "Address to bind to.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 4810;
      description = "Port to listen on.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the terrier port in the firewall.";
    };

    settings = lib.mkOption {
      type = settingsFormat.type;
      default = {};
      example = lib.literalExpression ''
        {
          scrape.renotify_drop_pct = 5.0;
          leboncoin = {
            enabled = true;
            queries = ["rtx 3080" "seagate ironwolf 4to"];
          };
          families = [
            {
              name = "nvidia-rtx";
              models = ["3070" "3080" "3090" "4080" "4090"];
            }
          ];
          notifications = {
            ntfy_url = "https://notify.zeus.balem.fr";
            topic = "deals-zeus";
            token_file = "/run/agenix/terrier-ntfy-token";
          };
          llm = {
            enabled = true;
            base_url = "http://127.0.0.1:8080/v1";
          };
        }
      '';
      description = ''
        terrier configuration, serialized to terrier.toml. See
        crates/terrier-server/terrier.example.toml for the available keys.
        Secrets stay out of the store: point token_file/api_key_file at
        agenix-managed paths and make them readable by the terrier user.
        listen, db_path and static_dir default to sane values below.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    services.terrier.settings = {
      listen = lib.mkDefault "${cfg.address}:${toString cfg.port}";
      db_path = lib.mkDefault "/var/lib/terrier/terrier.db";
      static_dir = lib.mkIf (cfg.webPackage != null) (lib.mkDefault cfg.webPackage);
    };

    systemd.services.terrier = {
      description = "terrier immobilier price tracker";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];

      # The Leboncoin source falls back to curl when DataDome
      # fingerprint-blocks the plain HTTP client.
      path = [pkgs.curl];

      environment.TERRIER_CONFIG = configFile;

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        User = "terrier";
        Group = "terrier";
        StateDirectory = "terrier";
        WorkingDirectory = "/var/lib/terrier";
        Restart = "on-failure";
        RestartSec = 5;

        # A privileged port (e.g. 80) needs the bind capability; the
        # service user is not root.
        AmbientCapabilities = lib.mkIf (cfg.port < 1024) ["CAP_NET_BIND_SERVICE"];
        CapabilityBoundingSet = lib.mkIf (cfg.port < 1024) ["CAP_NET_BIND_SERVICE"];

        # Hardening (the service only needs its state dir and the network).
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
      };
    };

    users.users.terrier = {
      isSystemUser = true;
      group = "terrier";
    };
    users.groups.terrier = {};

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [cfg.port];
  };
}

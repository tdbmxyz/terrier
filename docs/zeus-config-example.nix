# Ready-to-paste services.terrier block for /etc/nixos.
#
# Flake input:   terrier.url = "github:tdbmxyz/terrier";
# Import:        terrier.nixosModules.terrier
{
  services.terrier = {
    enable = true;
    openFirewall = true; # LAN/tailnet only — terrier has no auth

    settings = {
      scrape = {
        renotify_drop_pct = 1.0; # any ≥1% drop pings
        max_search_locations = 20;
      };

      # Leboncoin ventes_immobilières — searches created in the UI feed
      # their locations here automatically; the baseline just guarantees
      # traffic before the first search exists.
      leboncoin = {
        enabled = true;
        locations = ["Rennes 35000"];
        pages_per_location = 1;
        delay_ms = 3000;
        interval_minutes = 60;
      };

      # Ouest France Immo: bot-walled, needs the stealth fetcher first
      # (same Scrapling venv as ferret's eBay hook).
      ouestfrance = {
        enabled = false;
        locations = ["Rennes 35000"];
        # fetch_command = ["/var/lib/terrier/venv/bin/python" "/var/lib/terrier/stealth-fetch.py" "{url}"];
      };

      notifications = {
        ntfy_url = "https://notify.zeus.balem.fr";
        topic = "terrier";
      };
    };
  };
}

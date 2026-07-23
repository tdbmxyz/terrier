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

      # Detail-page enrichment: images + structured attributes on new
      # listings and price changes.
      enrichment = {
        poll_seconds = 60;
        max_attempts = 8;
        max_images = 10;
        images_dir = "images";
      };

      # LLM extraction via the zeus llama.cpp server. Fail-open: scraping
      # never depends on it.
      llm = {
        enabled = true;
        base_url = "http://127.0.0.1:8080/v1";
        model = "";
        timeout_secs = 120;
      };
    };
  };
}

# Phone Dispatch on Tailscale

Phone Dispatch serves HTTPS on a separate port on the hub's Tailscale IP. The phone's browser uses that address to load the page, read hub threads, and negotiate a voice call with the hub's signed-in Codex CLI. Enable MagicDNS and HTTPS certificates in the tailnet, and connect the phone to Tailscale.

On the hub, create a private directory and generate a Dispatch-only pairing code. Run these commands once so existing browsers can keep using their pairing code.

```sh
umask 077
mkdir -p "$HOME/.config/yonder/dispatch-tls"
chmod 700 "$HOME/.config/yonder/dispatch-tls"
cd "$HOME/.config/yonder/dispatch-tls"
openssl rand -hex 32 > pairing-code
```

Have a Tailscale operator run the following command there, using the hub's full MagicDNS hostname. Give the hub service account read access to the resulting files and keep `key.pem` private. Before the certificate expires, run the certificate command again and restart the hub to load the new certificate.

```sh
tailscale cert --cert-file cert.pem --key-file key.pem hub.example.ts.net
```

Add these flags to the hub's existing `yonder-hub serve` command, alongside `--dispatch-codex /path/to/codex`.

```sh
--dispatch-web-listen 100.64.0.10:8443 \
--dispatch-web-origin https://hub.example.ts.net:8443 \
--dispatch-web-directory /home/hub/.config/yonder/dispatch-tls
```

Replace the IP and hostname with the hub's Tailscale addresses. The tailnet policy must allow the phone to reach TCP port 8443. For a systemd user service, reload its configuration and restart the hub once no threads are running or waiting for input.

On the phone, open `https://hub.example.ts.net:8443/dispatch`, enter the code from `pairing-code`, and tap **Talk to Dispatch**. Allow microphone access and ask “What's running?” Tap **Hang up** to release the microphone. To revoke existing pairings, replace `pairing-code` with a new value and restart the hub.

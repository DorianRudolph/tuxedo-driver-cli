# tuxedo-driver-cli

Small root-friendly CLI for the TUXEDO driver features exposed on this XMG EVO
/ InfinityBook Pro Gen9 AMD class of hardware.

It reads and writes the driver interfaces directly:

- `/dev/tuxedo_io` for Uniwill fans and ODM performance profiles
- `/sys/devices/platform/tuxedo_keyboard` for charging profile, keyboard
  backlight, fn lock, AC auto boot, and USB powershare

Most write commands need root.

```sh
cargo build --release
sudo target/release/tuxedo-driver-cli status
sudo target/release/tuxedo-driver-cli fans set 1 35
sudo target/release/tuxedo-driver-cli fans auto
sudo target/release/tuxedo-driver-cli profile set overboost
sudo target/release/tuxedo-driver-cli charging set stationary
sudo target/release/tuxedo-driver-cli backlight set 2
sudo target/release/tuxedo-driver-cli usb-powershare set on
```

Run `tuxedo-driver-cli --help` and subcommand `--help` for the full command
surface.

## Daemon

The crate also builds `tuxedo-driver-daemon`, a small foreground systemd service
that applies optional startup settings from TOML and controls fans with a
configurable curve. On SIGTERM/SIGINT it asks the driver to restore firmware fan
auto mode.
The fan curve uses the primary fan temperature and applies the same target speed
to every detected fan.

Default config path:

```text
/etc/tuxedo-driver-daemon.toml
```

Example config:

```sh
sudo install -Dm0644 examples/tuxedo-driver-daemon.toml /etc/tuxedo-driver-daemon.toml
```

Manual test:

```sh
cargo build --release
sudo target/release/tuxedo-driver-daemon --config /etc/tuxedo-driver-daemon.toml
```

The systemd unit templates are in `systemd/`. Install both units so the daemon
is stopped before suspend and started again on resume:

```sh
sudo install -Dm0644 systemd/tuxedo-driver-daemon.service /etc/systemd/system/tuxedo-driver-daemon.service
sudo install -Dm0644 systemd/tuxedo-driver-daemon-sleep.service /etc/systemd/system/tuxedo-driver-daemon-sleep.service
sudo systemctl daemon-reload
sudo systemctl enable --now tuxedo-driver-daemon.service tuxedo-driver-daemon-sleep.service
```

## License

GPL-2.0-or-later. See `LICENSE`.

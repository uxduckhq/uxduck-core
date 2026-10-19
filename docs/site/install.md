---
title: Install
order: 2
group: Get started
summary: Install on macOS, Windows or Linux and check that UXDuck is running.
---

Install UXDuck to connect real vehicles over USB, radio or your network, and to keep things running without a browser tab.

Download for your computer from [uxduck.com/download](/download). Then:

## macOS

Open the `.pkg` and follow the installer.

## Windows

Run the `.msi`. If Windows says "Windows protected your PC", choose **More info**, then **Run anyway**.

## Linux and Raspberry Pi

```
sudo apt install ./uxduck-*.deb
systemctl --user start uxduck
```

The desktop window needs no graphics card, so it works in a virtual machine too. To start it with other Chromium switches, such as `--ozone-platform=wayland`, put them in `~/.config/uxduck-flags.conf`, then open UXDuck again.

## Open the app

Open **UXDuck** from Applications, Spotlight or the Start menu. It starts UXDuck
in the background and opens its desktop window. The installer finishes without
opening a window. You can also use:

```
uxduck open
```

In **Settings › Connections › When I open UXDuck**, choose **Open a window**,
**Open in browser** or **Run in menu bar only**. The menu bar has the same
preference and separate **Open window** and **Open in browser** actions. Changing
the preference opens nothing.

Check UXDuck is running with:

```
uxduck status
```

## The menu bar

While UXDuck runs, its icon is in the menu bar on a Mac, the notification area on Windows and the tray on Linux. From its menu you can check UXDuck is running, open UXDuck or these docs, send feedback with a screenshot, or quit. On GNOME the icon needs the AppIndicator extension. **Show UXDuck in the menu bar** in **Settings › UXDuck on this computer** turns it off.

**Start UXDuck at login** in **Settings › UXDuck on this computer**, or `uxduck service install`, starts UXDuck when you log in, without a window. `uxduck service uninstall` turns it off.

## Updates

UXDuck downloads new app versions in the background, including with its window closed. It asks before installing. Open **About › App updates** to check, install, or see what is waiting. Installing restarts UXDuck and may ask for your computer's administrator permission.

Choose **Install when idle** to wait for UXDuck’s windows and browser tabs to close and for vehicles, recordings, simulations and other work to finish. UXDuck also waits for a disconnected real vehicle last seen armed to reconnect and confirm it is disarmed. You can cancel the scheduled update; quitting UXDuck cancels it too. Updates never stop that work or restart your computer.

macOS uses its signed installer, Windows uses Windows Installer, and Debian/Ubuntu Linux uses its `.deb` package and polkit. A headless Linux computer can download updates but needs a manual package installation when no desktop authorization agent is available. Interface updates continue separately: **Reload** applies them to an open page.

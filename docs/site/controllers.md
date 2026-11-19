---
title: Controllers and radio transmitters
search: Tango 2, GX12, EdgeTX, Xbox, joystick axes, switches and missing buttons
order: 12.5
group: Use your vehicle
summary: Choose USB channels for a radio, check every input, and understand differences between Desktop and browser controller support.
---

Connect the controller and open the vehicle's Cockpit: the controller shows in its **Controls** subtab, under **Fly with**, once you move a stick, with **Set up** until its sticks are calibrated, then **Use**. Every controller, its mapping and its setup are under **Controllers**: **Set up controllers** beside **Fly with** or in the Cockpit before any vehicle is connected, the settings button beside the controller flying the vehicle, or **Set up controllers** in the command palette.

Open its setup, and check each input before assigning sticks or actions. Move one control at a time. Two controls intended for different actions should change different inputs.

## Axes and buttons depend on the device

An axis carries a range of values, including the three positions of a switch. A USB button carries pressed or released. A physical toggle can appear as either: the radio's USB configuration decides.

| Controller and USB mode                        | Axes                                   | Buttons                                | What to configure                                                                                                      |
| ---------------------------------------------- | -------------------------------------- | -------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| TBS Tango 2, FreedomTX classic joystick        | Model CH1–CH8                          | CH9–CH32 become Buttons 1–24           | Put each physical control on its own mixer channel. Use the first eight for sticks, knobs and three-position switches. |
| RadioMaster GX12, EdgeTX classic joystick      | First eight output channels            | Next 24 output channels                | Check the active model's mixes and channel order.                                                                      |
| GX12 with EdgeTX Advanced USB joystick enabled | Determined by the USB joystick mapping | Determined by the USB joystick mapping | Inspect the radio's advanced mapping; the classic channel boundary is not a universal rule.                            |
| Xbox gamepad                                   | Fixed sticks and triggers              | Fixed gamepad buttons                  | No radio mixer channels to reserve. UXDuck uses the recognized gamepad layout.                                         |

For a classic radio setup, reserve CH1–CH4 for the four stick functions and CH5–CH8 for other axes or three-position switches. CH9 onward suits momentary buttons and two-position switches. A button cannot preserve a three-position switch's middle state. These are USB input channels; the channels you later assign in UXDuck for vehicle control are a separate mapping.

EdgeTX recommends keeping the first eight channels as axes and buttons from channel nine when you want a model to work in both Classic and Advanced modes. Advanced settings and availability depend on the installed firmware. Reconnect USB after changing its joystick configuration. See [EdgeTX's joystick mapping reference](https://manual.edgetx.org/edgetx-how-to/joystick-mapping-information-for-game-developers) and [Advanced joystick setup](https://manual.edgetx.org/edgetx-how-to/configure-advanced-joystick-with-edgetx).

## TBS Tango 2

Select **USB Joystick (HID)** on the radio. This sends USB gamepad input; it is separate from USB serial and the SD-card/firmware volumes shown in storage mode.

On the active model, open **MIXES** and give each control a separate channel. Use a weight of **100**, offset **0**, and no switch condition for a direct switch mix. A switch condition can disable the mix instead of reporting all of its positions.

One example, keeping a model's existing four stick channels:

| Mixer channel | Source                        | What UXDuck receives                                 |
| ------------- | ----------------------------- | ---------------------------------------------------- |
| CH1–CH4       | Existing four stick functions | Axes 1–4; assign their actual functions during setup |
| CH5           | SA, top A toggle              | Axis 5, two positions                                |
| CH6           | SD, top D toggle              | Axis 6, two positions                                |
| CH7           | SB                            | Axis 7, three positions                              |
| CH8           | SC                            | Axis 8, three positions                              |
| CH9           | SE, rear E button             | Button 1                                             |
| CH10          | SF, rear F button             | Button 2                                             |

A and D are two-position toggles; E and F are momentary buttons. A/D appearing as axes is expected with the example above. To expose A or D as a USB button instead, assign it an unused channel from CH9 onward and update the UXDuck mapping. E/F will not be sent just because they exist physically: they also need mixes in the active model. Check the radio's channel monitor before looking for a missing input in UXDuck. See the [Tango 2 manual](https://www.team-blacksheep.com/media/files/tbs-tango2-manual.pdf).

### Two switches changing Input 7 on macOS

The Tango 2 tested with FreedomTX 1.4.0 describes CH7 and CH8 with the same USB HID usage, **Slider**, even though it sends two separate values. A reader that identifies an input only by that usage merges or drops a channel.

UXDuck's native macOS reader distinguishes these elements, keeping CH7's existing input code and assigning CH8 its own code. CH8 appears as **Axis 8**. This applies to unmapped controllers with repeated axis usages; recognized gamepads continue through their normal layout mappings.

Use UXDuck connected to **Computer** for this support, including when opening its interface in Chrome. A simulation running entirely in **Browser** still receives the browser's Gamepad API data. Chrome's macOS reader currently drops a second axis with the same usage; UXDuck cannot reconstruct a value the browser does not expose. [Chromium's macOS reader](https://chromium.googlesource.com/chromium/src/+/HEAD/device/gamepad/gamepad_device_mac.mm) documents that behavior.

After updating Desktop, repeat controller setup for CH8. Existing CH7 mappings retain their input code, but check which physical switch now drives any action previously learned while both switches were merged.

## Battery and rumble

A wireless controller's charge shows on its card under **Controllers** and on its row in the Cockpit's **Controls**, as the computer reports it: a percentage, **Charging**, **Charged** or **Wired**. A controller flying a vehicle raises a notice once when its battery is low, at 20% or less, and again once it is critical, at 5% or less. These are the levels UPower, the battery service on Linux desktops, warns of a device's battery at by default. The notice clears once the controller charges or reports more than the level.

A controller that can rumble confirms what it did without you looking at the screen:

- One short pulse when a held command, such as Arm, completes and goes to the vehicle.
- Two short pulses when UXDuck holds back a command from the controller, cannot send it, or the vehicle refuses it.
- One long pulse when the vehicle it flies is no longer heard, or its control pauses.

Only the controller flying a vehicle rumbles. Turn rumble off in **Settings** › **Controllers** › **Rumble**, which every window and the agent share.

What each computer reports:

| Where UXDuck runs | Battery                                                                                                                                                                                               | Rumble                                                                                    |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| Linux             | The charge and whether it is charging, where the controller's kernel driver reports it, such as hid-playstation, hid-nintendo, hid-steam or xpadneo. A Bluetooth controller without one says nothing. | Where the driver supports force feedback and UXDuck can write to the controller's device. |
| Windows           | The charge, charging or wired, as Windows reports it for the controller.                                                                                                                              | Xbox-compatible controllers.                                                              |
| macOS             | The percentage alone, where the controller reports one to macOS. macOS does not say whether it is charging or wired.                                                                                  | Not available.                                                                            |
| Browser           | Not available: the browser's Gamepad API has no battery, and the card says so.                                                                                                                        | Controllers the browser can rumble, through its Gamepad API, while UXDuck's tab is open.  |

Where a computer reports nothing, the card says **Not reported by this computer** and no battery notice comes, so check the controller's charge yourself before you fly.

## Check the result

1. Confirm the intended model is selected and its channel monitor changes for each control.
2. In UXDuck's controller setup, move B and C separately. Each should change only its own input, including the middle position.
3. Press and release E, then F. Each should change its own button if CH9/CH10 have the mixes above.
4. Unplug and reconnect the radio. Confirm the inputs and your saved assignments still match.

A missing mix needs a radio configuration change. Two distinct channel-monitor values reaching the same app input need investigation of the USB descriptor and reader. Include the radio model, firmware, USB mode, operating system, UXDuck version, and whether the app says **Browser** or **Computer** in a report. See [Troubleshooting](troubleshooting.md).

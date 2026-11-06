// What changed in each version, newest first: the "What's new" pop-up after an update and
// Settings → What's new show these.
const CHANGES = [
  ["2.3.0", [
    "Scenes: set your PC up how you like it and save it. One click brings it all back: your apps and where their windows were, theme, wallpaper, sound, lyrics and notifications. Find them in quick settings, Start and quick search",
    "Auto-hide the taskbar: it slides away and comes back when your mouse reaches the bottom of the screen. Press Win+B or right-click the taskbar to switch it on and off",
  ]],
  ["2.2.5", ["Much lighter on your PC: the Grove and Bubblegum decorations no longer keep animating in windows you can't see"]],
  ["2.2.4", ["The taskbar and lyrics stay in front of your apps, instead of sometimes ending up behind them"]],
  ["2.2.3", [
    "Buttons respond more reliably: their clickable areas stay up to date, and opening an app no longer freezes the taskbar",
    "The taskbar, lyrics and widgets fit your screen again when the resolution, display scale or monitors change",
  ]],
  ["2.2.2", ["Win+Tab opens Pink Cloud's switcher too, handy if your Alt and Windows keys are swapped. Want Windows' Task View back? Switch it off in Settings → Taskbar"]],
  ["2.2.1", ["Alt+Tab opens Pink Cloud's own switcher more reliably"]],
  ["2.2.0", [
    "New Bubblegum theme: hot pink and baby blue, candy stripes, and hearts, sparkles and bows that float up on your panels and idle screen",
    "Pink Cloud has its own Task Manager: see what's using your CPU and memory, and end stuck apps. Right-click the taskbar or find it in Start",
    "Lighter: the visualizer and lyrics use less of your PC, and rarely used screens only load when you need them",
    "Alt+Tab now looks like the rest of Pink Cloud's windows, and shows the name of the window you're about to pick",
  ]],
  ["2.1.0", [
    "Task Manager is one right-click away on the taskbar, and End task force-closes a stuck app",
    "Volume for each app: the sliders icon next to the volume in quick settings",
    "Keep any window on top: right-click its button on the taskbar",
    "App names on the taskbar, with one button per window: switch it on in Settings → Look and feel",
    "Clipboard history: type \"clip\" in quick search. It's off until you switch it on, and stays on your PC",
    "Taskbar buttons no longer swap places when you switch between apps",
  ]],
  ["2.0.1", [
    "Discord status is here: switch it on in Settings → Look and feel to show the song you're listening to on your Discord profile",
  ]],
  ["2.0.0", [
    "Safe start: if Pink Cloud ever crashes, your normal taskbar comes back right away, and you can go back to the version you had before an update",
    "\"Something's wrong?\" in Settings sends a bug report by email in two clicks",
    "Focus mode: notifications wait, lyrics step aside, and the taskbar counts down your session",
    "Achievements: 19 to collect, including a secret one",
    "Discord status: show the song you're listening to on your Discord profile",
    "A welcome tour for new friends, and this What's new pop-up after every update",
  ]],
  ["1.8.1", [
    "Grove: glowing glyphs fade in and out on your panels and idle screen",
    "Cosmos: the idle screen shows a real photo of the Milky Way",
  ]],
  ["1.8.0", ["New Grove theme: an old forest with sunbeams, drifting light and jade glyphs"]],
  ["1.7.3", ["Updating works again: Pink Cloud installs the update and opens again by itself"]],
  ["1.7.2", ["Open the normal Windows File Explorer when you want it"]],
  ["1.7.0", ["Game mode and overlay, quick search, wallpaper and custom themes, sound effects, and a much better Files"]],
];
// is version a newer than b ("1.10.0" > "1.9.2")
const newerVersion = (a, b) => {
  const x = a.split(".").map(Number), y = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) > (y[i] || 0);
  return false;
};

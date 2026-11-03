// What changed in each version, newest first: the "What's new" pop-up after an update and
// Settings → What's new show these.
const CHANGES = [
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

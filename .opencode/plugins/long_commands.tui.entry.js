// Entry shim for the TUI side of long_commands — the runtime-module import
// makes the host share its internal solid/opentui copies with this plugin
// (no local solid-js needed; if that scheme is missing, the import fails
// loudly and the plugin just doesn't load). Re-exports the component
// module's default, which the loader reads as the tui plugin object.
await import("opentui:runtime-module:" + encodeURIComponent("@opentui/solid"))
const mod = await import("./long_commands.tui.tsx")
export default mod.default

-- clipdeck for a Lua Hyprland config (hyprland.lua). Needs HOME defined, e.g.
-- local HOME = os.getenv("HOME")
-- >>> clipdeck
-- clipboard picker (SUPER+V): `clipdeck show` opens the overlay, then enters this submap.
-- Each entry's key copies it here; Esc / Enter / SUPER+V / any other key close it.
local clipdeck = HOME .. "/.local/bin/clipdeck "
local function clip_close()
    hl.exec_cmd(clipdeck .. "hide")
    hl.dispatch(hl.dsp.submap("reset"))
end
hl.define_submap("clip", function()
    for k in ("1234567890QWERTYUIOPASDFGHJKL"):gmatch(".") do
        hl.bind(k, hl.dsp.exec_cmd(clipdeck .. "pick " .. k))
    end
    hl.bind("Escape", clip_close)
    hl.bind("Return", clip_close)
    hl.bind("SUPER + V", clip_close)
    hl.bind("catchall", clip_close)  -- any other key closes it too: the submap can never trap you
end)
-- <<< clipdeck

-- and the key that opens it:
hl.bind("SUPER + V", hl.dsp.exec_cmd(HOME .. "/.local/bin/clipdeck show"))

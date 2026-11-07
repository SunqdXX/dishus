package org.dishus.app.input

/** One key on the on-screen PC keyboard. `code` is a Linux evdev keycode. */
data class Key(val label: String, val code: Int, val weight: Float = 1f, val shiftLabel: String? = null)

/** Linux input-event-codes.h values used by the keyboard. */
object Evdev {
    const val ESC = 1; const val BACKSPACE = 14; const val TAB = 15; const val ENTER = 28
    const val LEFTCTRL = 29; const val LEFTSHIFT = 42; const val RIGHTSHIFT = 54; const val LEFTALT = 56
    const val SPACE = 57; const val CAPSLOCK = 58; const val SYSRQ = 99; const val RIGHTALT = 100
    const val HOME = 102; const val UP = 103; const val PAGEUP = 104; const val LEFT = 105; const val RIGHT = 106
    const val END = 107; const val DOWN = 108; const val PAGEDOWN = 109; const val INSERT = 110
    const val DELETE = 111; const val RIGHTCTRL = 97; const val LEFTMETA = 125; const val COMPOSE = 127
    val F = intArrayOf(59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88) // F1..F12

    val MODIFIERS = setOf(LEFTCTRL, RIGHTCTRL, LEFTSHIFT, RIGHTSHIFT, LEFTALT, RIGHTALT, LEFTMETA)
}

/** US-QWERTY layout; the PC's XKB layout decides what the codes produce. */
object Layout {
    private fun row(s: String, codes: IntArray, shifted: String? = null) =
        s.mapIndexed { i, c -> Key(c.toString(), codes[i], shiftLabel = shifted?.getOrNull(i)?.toString()) }

    val rows: List<List<Key>> = listOf(
        listOf(Key("Esc", Evdev.ESC)) +
            (1..12).map { Key("F$it", Evdev.F[it - 1]) } +
            listOf(Key("PrtSc", Evdev.SYSRQ), Key("Del", Evdev.DELETE)),
        row("`1234567890-=", intArrayOf(41, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13), "~!@#$%^&*()_+") +
            Key("⌫", Evdev.BACKSPACE, 1.6f),
        listOf(Key("Tab", Evdev.TAB, 1.5f)) +
            row("qwertyuiop[]\\", intArrayOf(16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 43), "QWERTYUIOP{}|"),
        listOf(Key("Caps", Evdev.CAPSLOCK, 1.8f)) +
            row("asdfghjkl;'", intArrayOf(30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40), "ASDFGHJKL:\"") +
            Key("Enter", Evdev.ENTER, 1.9f),
        listOf(Key("Shift", Evdev.LEFTSHIFT, 2.2f)) +
            row("zxcvbnm,./", intArrayOf(44, 45, 46, 47, 48, 49, 50, 51, 52, 53), "ZXCVBNM<>?") +
            listOf(Key("Shift", Evdev.RIGHTSHIFT, 1.3f), Key("↑", Evdev.UP)),
        listOf(
            Key("Ctrl", Evdev.LEFTCTRL, 1.3f), Key("Super", Evdev.LEFTMETA, 1.2f), Key("Alt", Evdev.LEFTALT, 1.2f),
            Key("Space", Evdev.SPACE, 4.6f), Key("AltGr", Evdev.RIGHTALT, 1.2f), Key("Ctrl", Evdev.RIGHTCTRL, 1.2f),
            Key("←", Evdev.LEFT), Key("↓", Evdev.DOWN), Key("→", Evdev.RIGHT),
        ),
        listOf(
            Key("Ins", Evdev.INSERT), Key("Home", Evdev.HOME), Key("End", Evdev.END),
            Key("PgUp", Evdev.PAGEUP), Key("PgDn", Evdev.PAGEDOWN), Key("Menu", Evdev.COMPOSE),
        ),
    )
}

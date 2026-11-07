package org.dishus.app.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

val Accent = Color(0xFF7FD1AE)
val KeyBg = Color(0xFF1E2329)
val KeyBgPressed = Color(0xFF3A4450)
val KeySticky = Color(0xFF2F5D4A)
val KeyLocked = Color(0xFF7FD1AE)

private val scheme = darkColorScheme(
    primary = Accent,
    onPrimary = Color(0xFF07130D),
    background = Color(0xFF0E1114),
    surface = Color(0xFF14181C),
    surfaceVariant = Color(0xFF1E2329),
    onBackground = Color(0xFFE6EBEF),
    onSurface = Color(0xFFE6EBEF),
    error = Color(0xFFFF8A80),
)

@Composable
fun DishusTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = scheme, content = content)
}

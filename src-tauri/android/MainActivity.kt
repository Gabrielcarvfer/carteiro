package dev.carteiro.mail

import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.view.View
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

// Installed over the generated activity by scripts/prepare-android.sh.
class MainActivity : TauriActivity() {
  // Back is handled below instead of via WebView history: Chrome skips history
  // entries that weren't created by a user gesture, so Back would leave the app
  // with a conversation or dialog still open.
  override val handleBackNavigation: Boolean = false

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Edge-to-edge draws the page under the status and navigation bars, and
    // older WebViews don't report those insets to CSS. Pad the content by
    // the system bars, display cutout and keyboard instead, so nothing is
    // ever hidden behind them (and the keyboard never covers a text field).
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { v, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or WindowInsetsCompat.Type.ime()
      )
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }

  // Back asks the page to close its top layer (conversation, reply, dialog,
  // drawer); only when nothing is open does Android leave the app as usual.
  override fun onWebViewCreate(webView: WebView) {
    webView.addJavascriptInterface(Links(), "CarteiroAndroid")
    val callback = object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        webView.evaluateJavascript("window.carteiroBack ? window.carteiroBack() : false") { handled ->
          if (handled != "true") {
            isEnabled = false
            onBackPressedDispatcher.onBackPressed()
            isEnabled = true
          }
        }
      }
    }
    onBackPressedDispatcher.addCallback(this, callback)
  }

  // Opens web and mail links in the user's browser / mail app. Same rule as
  // the desktop shell: http, https and mailto only, no control or
  // text-direction characters. Mail bodies render in a frame with scripts
  // disabled, so only Carteiro's own page can call this.
  inner class Links {
    @JavascriptInterface
    fun open(url: String): Boolean {
      val lower = url.lowercase()
      val allowed = url.length < 8192 &&
        (lower.startsWith("https://") || lower.startsWith("http://") || lower.startsWith("mailto:")) &&
        url.none { it.isISOControl() || it in '\u202A'..'\u202E' || it in '\u2066'..'\u2069' || it == '\u200E' || it == '\u200F' || it == ' ' }
      if (!allowed) return false
      return try {
        startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)).addCategory(Intent.CATEGORY_BROWSABLE))
        true
      } catch (e: ActivityNotFoundException) {
        false
      }
    }
  }
}

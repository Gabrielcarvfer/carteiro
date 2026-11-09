package dev.carteiro.mail

import android.Manifest
import android.content.ActivityNotFoundException
import android.content.pm.PackageManager
import android.provider.ContactsContract
import org.json.JSONArray
import org.json.JSONObject
import android.media.MediaScannerConnection
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

  private var web: WebView? = null
  private val contactsRequest = 7101

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Mail checks and notifications while Carteiro is in the background.
    MailJobService.schedule(this)
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
    web = webView
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

  @Deprecated("TauriActivity doesn't offer the activity result API")
  override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (requestCode == contactsRequest) {
      val granted = grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED
      web?.post { web?.evaluateJavascript("window.carteiroContactsPermission && window.carteiroContactsPermission($granted)", null) }
    }
  }

  // Opens web and mail links in the user's browser / mail app. Same rule as
  // the desktop shell: http, https and mailto only, no control or
  // text-direction characters. Mail bodies render in a frame with scripts
  // disabled, so only Carteiro's own page can call this.
  inner class Links {
    // Saved files appear in Files / Downloads only once Android's media
    // index knows them. Only Carteiro's own Download/Carteiro folder.
    @JavascriptInterface
    fun indexFile(path: String): Boolean {
      val file = java.io.File(path).canonicalFile
      val dir = java.io.File("/storage/emulated/0/Download/Carteiro").canonicalFile
      if (!file.path.startsWith(dir.path + "/")) return false
      val paths = if (file.isDirectory) file.walk().filter { it.isFile }.map { it.path }.toList().toTypedArray() else arrayOf(file.path)
      MediaScannerConnection.scanFile(this@MainActivity, paths, null, null)
      return true
    }

    // The phone's contacts, only with the user's permission (asked when they
    // choose to bring them in). Read-only: Carteiro never changes them.
    @JavascriptInterface
    fun contactsAllowed(): Boolean =
      checkSelfPermission(Manifest.permission.READ_CONTACTS) == PackageManager.PERMISSION_GRANTED

    @JavascriptInterface
    fun askContacts() {
      runOnUiThread { requestPermissions(arrayOf(Manifest.permission.READ_CONTACTS), contactsRequest) }
    }

    /** [[lookup key, {name, emails, phones, organization}], ...] as JSON. */
    @JavascriptInterface
    fun readContacts(): String {
      if (!contactsAllowed()) return "[]"
      val people = LinkedHashMap<String, JSONObject>()
      val kinds = arrayOf(
        ContactsContract.CommonDataKinds.Email.CONTENT_ITEM_TYPE,
        ContactsContract.CommonDataKinds.Phone.CONTENT_ITEM_TYPE,
        ContactsContract.CommonDataKinds.Organization.CONTENT_ITEM_TYPE,
        ContactsContract.CommonDataKinds.StructuredName.CONTENT_ITEM_TYPE
      )
      val cols = arrayOf(
        ContactsContract.Data.LOOKUP_KEY, ContactsContract.Data.DISPLAY_NAME, ContactsContract.Data.MIMETYPE,
        ContactsContract.Data.DATA1
      )
      contentResolver.query(
        ContactsContract.Data.CONTENT_URI, cols, "${ContactsContract.Data.MIMETYPE} IN (?, ?, ?, ?)", kinds, null
      )?.use { c ->
        while (c.moveToNext()) {
          val key = c.getString(0) ?: continue
          val p = people.getOrPut(key) {
            JSONObject().put("name", c.getString(1) ?: "").put("emails", JSONArray()).put("phones", JSONArray()).put("organization", "")
          }
          val value = c.getString(3)?.trim().orEmpty()
          if (value.isEmpty()) continue
          when (c.getString(2)) {
            kinds[0] -> p.getJSONArray("emails").put(value)
            kinds[1] -> p.getJSONArray("phones").put(value)
            kinds[2] -> p.put("organization", value)
          }
        }
      }
      val out = JSONArray()
      for ((key, p) in people) out.put(JSONArray().put(key).put(p))
      return out.toString()
    }

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

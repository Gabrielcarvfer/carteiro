package dev.carteiro.mail

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat
import org.json.JSONObject

// Installed next to MainActivity by scripts/prepare-android.sh.
//
// Carteiro's work while it's in the background or closed, through Android's
// own job scheduler (no extra library): checking mail every 15 minutes and
// notifying, and, when the phone is charging, idle and on Wi-Fi (at night),
// downloading the offline period and checking it with ClamAV ahead of time.
// The work itself is the app's Rust engine (background_run in lib.rs).
class MailJobService : JobService() {
  companion object {
    private const val CHECK = 7201
    private const val NIGHT = 7202
    private const val CHANNEL = "new-mail"

    init {
      System.loadLibrary("carteiro_lib")
    }

    /** Registers both jobs (again on every start: cheap, and survives updates). */
    fun schedule(context: Context) {
      // Never let scheduling stop the app from opening.
      try { register(context) } catch (e: Exception) { }
    }

    private fun register(context: Context) {
      val js = context.getSystemService(JobScheduler::class.java) ?: return
      val me = ComponentName(context, MailJobService::class.java)
      val persisted = context.checkSelfPermission(Manifest.permission.RECEIVE_BOOT_COMPLETED) == PackageManager.PERMISSION_GRANTED
      js.schedule(
        JobInfo.Builder(CHECK, me)
          .setPeriodic(15 * 60 * 1000L)
          .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
          .setPersisted(persisted)
          .build()
      )
      js.schedule(
        JobInfo.Builder(NIGHT, me)
          .setPeriodic(6 * 60 * 60 * 1000L)
          .setRequiresCharging(true)
          .setRequiresDeviceIdle(true)
          .setRequiredNetworkType(JobInfo.NETWORK_TYPE_UNMETERED)
          .setPersisted(persisted)
          .build()
      )
    }
  }

  private external fun runInBackground(dataDir: String, deep: Boolean): String?

  override fun onStartJob(params: JobParameters): Boolean {
    val deep = params.jobId == NIGHT
    Thread {
      try {
        val report = JSONObject(runInBackground(applicationInfo.dataDir, deep) ?: "{}")
        val notes = report.optJSONArray("notify") ?: org.json.JSONArray()
        val log = report.optJSONArray("log") ?: org.json.JSONArray()
        for (i in 0 until log.length()) Log.i("Carteiro", "background ${if (deep) "night" else "check"}: ${log.optString(i)}")
        for (i in 0 until notes.length()) {
          val n = notes.getJSONArray(i)
          notify(n.optString(0), n.optString(1))
        }
      } catch (e: Throwable) {
        // A failed run is tried again at the next interval.
        Log.w("Carteiro", "background run failed", e)
      } finally {
        jobFinished(params, false)
      }
    }.start()
    return true
  }

  // Android may stop a job early (lost network, timeout): run it again later.
  override fun onStopJob(params: JobParameters): Boolean = true

  private fun notify(title: String, text: String) {
    if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) return
    val nm = getSystemService(NotificationManager::class.java) ?: return
    nm.createNotificationChannel(NotificationChannel(CHANNEL, "New mail", NotificationManager.IMPORTANCE_DEFAULT))
    val open = PendingIntent.getActivity(
      this, 0,
      Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
      PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
    )
    val n = NotificationCompat.Builder(this, CHANNEL)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(title)
      .setContentText(text)
      .setContentIntent(open)
      .setAutoCancel(true)
      .build()
    nm.notify(title.hashCode() xor text.hashCode(), n)
  }
}

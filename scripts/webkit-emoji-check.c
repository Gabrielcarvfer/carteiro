// Draws a page of color emoji in an off-screen WebKitGTK view and reports
// whether WebKit's web process survived it and how many colored pixels the
// page shows (emoji drawn in color, not as empty boxes). Built and run by
// scripts/appimage-font-check.sh against the AppImage's own WebKit.
#include <webkit2/webkit2.h>
#include <stdlib.h>

static int crashed = 0;
static long colored = -1;

static void on_terminated(WebKitWebView *view, WebKitWebProcessTerminationReason reason, gpointer data) {
  crashed = 1;
  gtk_main_quit();
}

static void on_snapshot(GObject *view, GAsyncResult *res, gpointer data) {
  cairo_surface_t *s = webkit_web_view_get_snapshot_finish(WEBKIT_WEB_VIEW(view), res, NULL);
  if (s) {
    cairo_surface_flush(s);
    int w = cairo_image_surface_get_width(s), h = cairo_image_surface_get_height(s), stride = cairo_image_surface_get_stride(s);
    unsigned char *px = cairo_image_surface_get_data(s);
    colored = 0;
    for (int y = 0; y < h; y++)
      for (int x = 0; x < w; x++) {
        unsigned char *p = px + y * stride + x * 4;
        int r = p[2], g = p[1], b = p[0];
        int hi = r > g ? (r > b ? r : b) : (g > b ? g : b), lo = r < g ? (r < b ? r : b) : (g < b ? g : b);
        if (hi - lo > 60) colored++;
      }
    cairo_surface_destroy(s);
  }
  gtk_main_quit();
}

static gboolean take_snapshot(gpointer view) {
  webkit_web_view_get_snapshot(WEBKIT_WEB_VIEW(view), WEBKIT_SNAPSHOT_REGION_VISIBLE, WEBKIT_SNAPSHOT_OPTIONS_NONE, NULL, on_snapshot, NULL);
  return FALSE;
}

static gboolean give_up(gpointer data) {
  gtk_main_quit();
  return FALSE;
}

int main(int argc, char **argv) {
  gtk_init(&argc, &argv);
  GtkWidget *win = gtk_offscreen_window_new();
  gtk_window_set_default_size(GTK_WINDOW(win), 640, 200);
  GtkWidget *view = webkit_web_view_new();
  gtk_container_add(GTK_CONTAINER(win), view);
  g_signal_connect(view, "web-process-terminated", G_CALLBACK(on_terminated), NULL);
  webkit_web_view_load_html(WEBKIT_WEB_VIEW(view),
    "<body style='margin:0;background:#fff'><p style='font-size:48px;margin:8px'>"
    "\xF0\x9F\x98\x80\xF0\x9F\x8E\x89\xF0\x9F\x91\x8D\xE2\x9D\xA4\xEF\xB8\x8F\xF0\x9F\x9A\x80"
    "\xF0\x9F\x8C\x8D\xE2\x9C\x85\xF0\x9F\x93\x85\xF0\x9F\x8C\x88\xF0\x9F\xA6\x84</p></body>", NULL);
  gtk_widget_show_all(win);
  int wait = argc > 1 ? atoi(argv[1]) : 20;
  g_timeout_add_seconds(3, take_snapshot, view);
  g_timeout_add_seconds(wait, give_up, NULL);
  gtk_main();
  if (crashed) puts("web process crashed");
  else printf("web process alive, %ld colored pixels\n", colored);
  return crashed;
}

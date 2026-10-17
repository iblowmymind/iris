/* toktest: one GL feature a step, to see which command-processor tokens
 * libGLcore sends for it (run under `mgras trace F hq,rss,cpu`).
 *
 *   toktest [STEP...]     (no steps: all of them, in this order)
 *
 * Each step draws into a 256 x 256 RGBA window and prints its name, then
 * calls glFinish, so the trace splits at the GL_FINISH readbacks:
 *   points-smooth   GL_POINT_SMOOTH points, then plain ones
 *   tex             a 16 x 16 RGBA texture on a quad
 *   tex-sub         glTexSubImage2D into it
 *   tex-table       the same texture with GL_TEXTURE_COLOR_TABLE_SGI
 *   drawpix-tex     glDrawPixels with GL_TEXTURE_2D enabled
 *   copypix-tex     glCopyPixels with GL_TEXTURE_2D enabled
 *   copyteximage    glCopyTexImage2D from the window
 *   lines-smooth    GL_LINE_SMOOTH lines
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <X11/Xlib.h>
#include <GL/gl.h>
#include <GL/glx.h>

static GLubyte tex[16 * 16 * 4];
static GLuint name;

static void draw_quad(void)
{
    glBegin(GL_QUADS);
    glTexCoord2f(0, 0); glVertex2f(20, 20);
    glTexCoord2f(1, 0); glVertex2f(120, 20);
    glTexCoord2f(1, 1); glVertex2f(120, 120);
    glTexCoord2f(0, 1); glVertex2f(20, 120);
    glEnd();
}

static void load_tex(void)
{
    int i;
    for (i = 0; i < 16 * 16; i++) {
        tex[i * 4] = (GLubyte)(i * 16);
        tex[i * 4 + 1] = (GLubyte)(255 - i);
        tex[i * 4 + 2] = (GLubyte)((i / 16) * 16);
        tex[i * 4 + 3] = 0xFF;
    }
    glBindTexture(GL_TEXTURE_2D, name);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 16, 16, 0, GL_RGBA, GL_UNSIGNED_BYTE, tex);
}

static int step(const char *s)
{
    int i;
    GLubyte img[32 * 32 * 4];
    for (i = 0; i < 32 * 32 * 4; i++)
        img[i] = (GLubyte)(i * 7);
    glClearColor(0.1f, 0.2f, 0.3f, 1);
    glClear(GL_COLOR_BUFFER_BIT);
    if (!strcmp(s, "points-smooth")) {
        glPointSize(9);
        glEnable(GL_POINT_SMOOTH);
        glEnable(GL_BLEND);
        glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
        glBegin(GL_POINTS);
        glColor3f(1, 1, 0);
        glVertex2f(50, 50);
        glEnd();
        glDisable(GL_POINT_SMOOTH);
        glDisable(GL_BLEND);
        glBegin(GL_POINTS);
        glVertex2f(100, 50);
        glEnd();
        glPointSize(1);
    } else if (!strcmp(s, "lines-smooth")) {
        glEnable(GL_LINE_SMOOTH);
        glEnable(GL_BLEND);
        glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
        glLineWidth(3);
        glBegin(GL_LINES);
        glColor3f(1, 1, 1);
        glVertex2f(10, 10);
        glVertex2f(200, 150);
        glEnd();
        glLineWidth(1);
        glDisable(GL_LINE_SMOOTH);
        glDisable(GL_BLEND);
    } else if (!strcmp(s, "tex")) {
        load_tex();
        glEnable(GL_TEXTURE_2D);
        glColor3f(1, 1, 1);
        draw_quad();
        glDisable(GL_TEXTURE_2D);
    } else if (!strcmp(s, "tex-sub")) {
        load_tex();
        glTexSubImage2D(GL_TEXTURE_2D, 0, 4, 4, 8, 8, GL_RGBA, GL_UNSIGNED_BYTE, img);
        glEnable(GL_TEXTURE_2D);
        draw_quad();
        glDisable(GL_TEXTURE_2D);
    } else if (!strcmp(s, "tex-table")) {
#ifdef GL_TEXTURE_COLOR_TABLE_SGI
        static GLubyte table[256 * 4];
        for (i = 0; i < 256 * 4; i++)
            table[i] = (GLubyte)(255 - i / 4);
        load_tex();
        glColorTableSGI(GL_TEXTURE_COLOR_TABLE_SGI, GL_RGBA, 256, GL_RGBA, GL_UNSIGNED_BYTE, table);
        glEnable(GL_TEXTURE_COLOR_TABLE_SGI);
        glEnable(GL_TEXTURE_2D);
        draw_quad();
        glDisable(GL_TEXTURE_2D);
        glDisable(GL_TEXTURE_COLOR_TABLE_SGI);
#else
        printf("no GL_TEXTURE_COLOR_TABLE_SGI\n");
#endif
    } else if (!strcmp(s, "drawpix-tex")) {
        load_tex();
        glEnable(GL_TEXTURE_2D);
        glTexCoord2f(0.5f, 0.5f);
        glRasterPos2i(40, 40);
        glDrawPixels(32, 32, GL_RGBA, GL_UNSIGNED_BYTE, img);
        glDisable(GL_TEXTURE_2D);
    } else if (!strcmp(s, "copypix-tex")) {
        glRasterPos2i(10, 10);
        glDrawPixels(32, 32, GL_RGBA, GL_UNSIGNED_BYTE, img);
        load_tex();
        glEnable(GL_TEXTURE_2D);
        glTexCoord2f(0.5f, 0.5f);
        glRasterPos2i(100, 100);
        glCopyPixels(10, 10, 32, 32, GL_COLOR);
        glDisable(GL_TEXTURE_2D);
    } else if (!strcmp(s, "copyteximage")) {
        glRasterPos2i(10, 10);
        glDrawPixels(32, 32, GL_RGBA, GL_UNSIGNED_BYTE, img);
        glBindTexture(GL_TEXTURE_2D, name);
        glCopyTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 10, 10, 32, 32, 0);
        glEnable(GL_TEXTURE_2D);
        draw_quad();
        glDisable(GL_TEXTURE_2D);
    } else {
        fprintf(stderr, "toktest: unknown step %s\n", s);
        return 1;
    }
    glFinish();
    printf("step %s done (GL error 0x%x)\n", s, (unsigned)glGetError());
    fflush(stdout);
    return 0;
}

int main(int argc, char **argv)
{
    static const char *all[] = { "points-smooth", "lines-smooth", "tex", "tex-sub", "tex-table", "drawpix-tex",
                                 "copypix-tex", "copyteximage" };
    int attr[] = { GLX_RGBA, GLX_RED_SIZE, 8, GLX_GREEN_SIZE, 8, GLX_BLUE_SIZE, 8, GLX_ALPHA_SIZE, 8, None };
    Display *dpy = XOpenDisplay(NULL);
    XVisualInfo *vi;
    GLXContext cx;
    XSetWindowAttributes swa;
    Window win;
    XEvent ev;
    int i, bad = 0;
    if (!dpy || !(vi = glXChooseVisual(dpy, DefaultScreen(dpy), attr))) {
        fprintf(stderr, "toktest: no display or visual\n");
        return 99;
    }
    cx = glXCreateContext(dpy, vi, NULL, True);
    swa.colormap = XCreateColormap(dpy, RootWindow(dpy, vi->screen), vi->visual, AllocNone);
    swa.border_pixel = 0;
    swa.event_mask = StructureNotifyMask;
    win = XCreateWindow(dpy, RootWindow(dpy, vi->screen), 60, 60, 256, 256, 0, vi->depth, InputOutput, vi->visual,
                        CWBorderPixel | CWColormap | CWEventMask, &swa);
    XStoreName(dpy, win, "toktest");
    XMapWindow(dpy, win);
    do
        XNextEvent(dpy, &ev);
    while (ev.type != MapNotify);
    glXMakeCurrent(dpy, win, cx);
    glViewport(0, 0, 256, 256);
    glMatrixMode(GL_PROJECTION);
    glLoadIdentity();
    glOrtho(0, 256, 0, 256, -1, 1);
    glMatrixMode(GL_MODELVIEW);
    glLoadIdentity();
    glGenTextures(1, &name);
    if (argc > 1)
        for (i = 1; i < argc; i++)
            bad += step(argv[i]);
    else
        for (i = 0; i < (int)(sizeof all / sizeof all[0]); i++)
            bad += step(all[i]);
    return bad;
}

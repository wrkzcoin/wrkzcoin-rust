// Copyright (c) 2026, The WrkzCoin developers
//
// Please see the included LICENSE file for more information

/*
 * <sys/timeb.h> for Apple targets built without Apple's SDK.
 *
 * The vendored c/oaes_lib.c (line 46) includes <sys/timeb.h> on every system
 * but FreeBSD, OpenBSD and Android, for the one ftime() call that seeds OAES
 * key generation (oaes_get_seed, line 679) — not the CryptoNight hash. Apple's
 * SDK still ships this long-deprecated header, but the macOS headers zig
 * carries do not, so a macOS build from Linux (cargo zigbuild) cannot compile
 * the file. The vendored C is never edited (build.rs), hence this header,
 * which build.rs puts on the include path for Apple targets only.
 *
 * struct timeb is the BSD layout Apple's own header declares. ftime() is
 * implemented over gettimeofday() rather than declared, so the link needs no
 * _ftime from libSystem, and it returns what ftime() returns: the time to the
 * millisecond, with the obsolete timezone and dstflag fields zeroed (Apple's
 * ftime() leaves them unset as well).
 */
#ifndef WRKZ_COMPAT_SYS_TIMEB_H
#define WRKZ_COMPAT_SYS_TIMEB_H

#include <sys/time.h>
#include <time.h>

struct timeb
{
    time_t time;
    unsigned short millitm;
    short timezone;
    short dstflag;
};

static inline int ftime(struct timeb *tp)
{
    struct timeval tv;
    if (gettimeofday(&tv, 0) != 0)
    {
        return -1;
    }
    tp->time = tv.tv_sec;
    tp->millitm = (unsigned short)(tv.tv_usec / 1000);
    tp->timezone = 0;
    tp->dstflag = 0;
    return 0;
}

#endif /* WRKZ_COMPAT_SYS_TIMEB_H */

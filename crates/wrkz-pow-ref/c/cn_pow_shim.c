/* The C++ half of the vendored CryptoNight code, in C. Built only with the
 * `pow` feature, where the proof-of-work C is compiled as a test oracle.
 *
 * slow-hash-x86.c keeps its 128KiB..2MiB scratchpad in a C thread-local
 * between hashes, and a C thread-local has no destructor. Upstream hangs the
 * release off a C++ thread_local (slow-hash-state.cpp); this port has no C++,
 * so the same job is done with a TLS destructor: the key's only value is a
 * sentinel, and the runtime calls the destructor on thread exit.
 *
 * The guard mirrors slow-hash-x86.c:11 so this file does not reference
 * slow_hash_release_state on the ARM and portable targets, which never
 * define it. */
#if !defined NO_AES && (defined(__x86_64__) || (defined(_MSC_VER) && defined(_WIN64)))

extern void slow_hash_release_state(void);

void wrkz_release_scratchpad(void)
{
    slow_hash_release_state();
}

#if defined(_WIN32)
#include <windows.h>
static DWORD wrkz_tls_index = FLS_OUT_OF_INDEXES;
static INIT_ONCE wrkz_tls_once = INIT_ONCE_STATIC_INIT;
static void WINAPI wrkz_tls_dtor(void *unused)
{
    (void)unused;
    slow_hash_release_state();
}
static BOOL CALLBACK wrkz_tls_init(PINIT_ONCE o, PVOID p, PVOID *c)
{
    (void)o;
    (void)p;
    (void)c;
    wrkz_tls_index = FlsAlloc(wrkz_tls_dtor);
    return TRUE;
}
void slow_hash_arm_state_release(void)
{
    InitOnceExecuteOnce(&wrkz_tls_once, wrkz_tls_init, NULL, NULL);
    if (wrkz_tls_index != FLS_OUT_OF_INDEXES && FlsGetValue(wrkz_tls_index) == NULL)
    {
        FlsSetValue(wrkz_tls_index, (void *)1);
    }
}
#else
#include <pthread.h>
static pthread_key_t wrkz_tls_key;
static pthread_once_t wrkz_tls_once = PTHREAD_ONCE_INIT;
static void wrkz_tls_dtor(void *unused)
{
    (void)unused;
    slow_hash_release_state();
}
static void wrkz_tls_init(void)
{
    pthread_key_create(&wrkz_tls_key, wrkz_tls_dtor);
}
void slow_hash_arm_state_release(void)
{
    pthread_once(&wrkz_tls_once, wrkz_tls_init);
    if (pthread_getspecific(wrkz_tls_key) == NULL)
    {
        pthread_setspecific(wrkz_tls_key, (void *)1);
    }
}
#endif

#else /* no kept scratchpad on this target */

void wrkz_release_scratchpad(void) {}
void slow_hash_arm_state_release(void) {}

#endif

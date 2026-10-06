#include <stdint.h>
#include <stdlib.h>
#include <time.h>
int64_t benchmark_clock_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) abort();
    return (int64_t)value.tv_sec * INT64_C(1000000000) + value.tv_nsec;
}

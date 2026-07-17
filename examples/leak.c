#include <stdlib.h>

/* 関数終端まで free も脱出もしない -> リーク疑い */
void leaky(void) {
    int *nums = malloc(10 * sizeof(int));
    nums[0] = 42;
}

/* 所有中に再確保で上書き -> 旧資源が迷子 */
void overwrite(void) {
    char *p = malloc(16);
    p = malloc(32);
    free(p);
}

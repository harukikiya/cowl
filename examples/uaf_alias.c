#include <stdlib.h>

/* 別名(q)経由で free した後に元の名前(p)で使用 -> UAF */
void alias_uaf(void) {
    char *p = malloc(8);
    char *q = p;
    free(q);
    char c = *p;
    (void)c;
}

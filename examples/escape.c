#include <stdlib.h>

/* return で所有権が呼び出し元へ脱出 -> リークではない */
char *make_buf(void) {
    char *b = malloc(128);
    return b;
}

/* 未知関数へ渡す -> 消費するか分からない = 曖昧 */
extern void register_buffer(char *b);
void ambiguous(void) {
    char *b = malloc(128);
    register_buffer(b);
    free(b);
}

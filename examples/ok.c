#include <stdlib.h>
#include <string.h>

/* 教科書的なライフサイクル: 確保 -> 使用 -> 解放 */
void textbook(void) {
    char *buf = malloc(64);
    if (!buf) return;
    strcpy(buf, "hello");
    free(buf);
    buf = NULL;
}

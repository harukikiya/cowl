#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* [1] ストリームの教科書コース: fopen -> 使用 -> fclose
       （W3: FILE* もヒープ資源と同様にライフタイム帯が出る） */
void stream_ok(const char *path) {
    FILE *fp = fopen(path, "r");
    if (!fp) return;
    char buf[64];
    fgets(buf, sizeof buf, fp);
    fclose(fp);
}

/* [2] fclose 忘れ: FILE* のリーク疑い */
void stream_leak(const char *path) {
    FILE *fp = fopen(path, "w");
    if (!fp) return;
    fputs("hello", fp);
}

/* [3] realloc の定石形: 旧領域は realloc が消費し、新領域を再獲得する。
       W3 のイベント順序修正（右辺評価→代入の実行意味論）前は
       偽の overwrite_owned / leak_suspect が出ていたケース */
void grow(void) {
    char *p = malloc(4);
    if (!p) return;
    p = realloc(p, 8);
    free(p);
}

/* [4] strdup の引数は借用: q が新しい所有を得るが p の所有は残る
       （W3: strdup を BENIGN にも載せたことで引数側の「曖昧」が解消） */
void dup_keeps_source(void) {
    char *p = malloc(4);
    strcpy(p, "ab");
    char *q = strdup(p);
    free(q);
    free(p);
}

typedef unsigned long usize;
typedef long isize;

static isize syscall3(long number, long a, long b, long c) {
    isize result;
    __asm__ volatile("syscall" : "=a"(result) : "a"(number), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return result;
}

static void write_all(const char *text, usize length) {
    while (length) {
        isize written = syscall3(1, 1, (long)text, (long)length);
        if (written <= 0) syscall3(60, 70, 0, 0);
        text += written;
        length -= (usize)written;
    }
}

static usize length(const char *text) {
    usize size = 0;
    while (text[size]) ++size;
    return size;
}

static int equals(const char *left, const char *right) {
    while (*left && *left == *right) { ++left; ++right; }
    return *left == *right;
}

static int contains(const char *text, const char *needle) {
    for (; *text; ++text) {
        const char *a = text;
        const char *b = needle;
        while (*a && *b && *a == *b) { ++a; ++b; }
        if (!*b) return 1;
    }
    return 0;
}

static const char *env_value(char **environment, const char *name) {
    usize name_length = length(name);
    for (; *environment; ++environment) {
        usize index = 0;
        while (index < name_length && (*environment)[index] == name[index]) ++index;
        if (index == name_length && (*environment)[index] == '=') return *environment + index + 1;
    }
    return (const char *)0;
}

static void emit_rpc(const char *id, usize id_length, const char *result) {
    write_all("{\"id\":", 6);
    write_all(id, id_length);
    write_all(",\"result\":", 10);
    write_all(result, length(result));
    write_all("}\n", 2);
}

static int run(long argc, char **argv, char **environment) {
    if (argc == 2 && equals(argv[1], "--offline-probe")) {
        const char *probe = "{\"schema\":\"test.selected_runtime_probe.v1\",\"marker\":\"selected-runtime-program-v1\"}\n";
        write_all(probe, length(probe));
        return 0;
    }
    if (argc == 3 && equals(argv[1], "--qualification-probe") && length(argv[2]) == 64) {
        const char *head = "{\"schema\":\"ryeos.product_qualification_result.v1\",\"subject_manifest_hash\":\"";
        write_all(head, length(head));
        write_all(argv[2], 64);
        const char *tail = "\",\"claims\":[\"runtime_program_executed\"],\"probe_evidence\":{\"schema\":\"test.selected_runtime_probe.v1\",\"marker\":\"selected-runtime-program-v1\"}}\n";
        write_all(tail, length(tail));
        return 0;
    }

    char request[4096];
    usize used = 0;
    for (;;) {
        char byte;
        isize count = syscall3(0, 0, (long)&byte, 1);
        if (count == 0) return 0;
        if (count < 0) return 74;
        if (byte != '\n') {
            if (used + 1 >= sizeof request) return 65;
            request[used++] = byte;
            continue;
        }
        request[used] = 0;
        const char *id = request;
        while (*id && !(id[0] == '\"' && id[1] == 'i' && id[2] == 'd' && id[3] == '\"' && id[4] == ':')) ++id;
        if (!*id) return 65;
        id += 5;
        const char *end = id;
        while (*end >= '0' && *end <= '9') ++end;
        if (end == id) return 65;
        if (contains(request, "\"method\":\"initialize\""))
            emit_rpc(id, (usize)(end - id), "{\"ready\":true}");
        else if (contains(request, "\"method\":\"fixture/credential/start\""))
            emit_rpc(id, (usize)(end - id), "{\"login_id\":\"fixture-login-v1\"}");
        else if (contains(request, "\"method\":\"fixture/credential/read\""))
            emit_rpc(id, (usize)(end - id), "{\"account\":{\"email\":\"offline@example.test\",\"type\":\"fixture\"}}");
        else if (contains(request, "\"method\":\"fixture/session/run\""))
            emit_rpc(id, (usize)(end - id), "{\"schema\":\"test.selected_runtime_execution.v1\",\"marker\":\"selected-runtime-program-v1\",\"network_contacted\":false}");
        else
            return 65;
        used = 0;
    }
}

__attribute__((noreturn)) void _start(void) {
    long *stack;
    __asm__ volatile("mov %%rsp,%0" : "=r"(stack));
    long argc = *stack;
    char **argv = (char **)(stack + 1);
    char **environment = argv + argc + 1;
    syscall3(60, run(argc, argv, environment), 0, 0);
    __builtin_unreachable();
}

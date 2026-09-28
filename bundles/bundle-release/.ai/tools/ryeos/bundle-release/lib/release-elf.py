# ryeos:signed:2026-09-23T04:29:46Z:8b9aa3dd2eeb70e96185f9a1875136f7d81a28dc7504ac2afe0f79442eadd4b2:/6Z6e3955VC+3a9jiyalQgjXbB297jvRVrIXnhTM2Hp2uFkwBU3wCg8jygaR3m6jgjWZ63p25iCkMIg3vCIPDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Closed ELF contract for copied, owned release payloads (not build tools)."""
import hashlib
import os
import pathlib
import stat
import struct

SUBSTRATE_INTERPRETER = "/lib64/ld-linux-x86-64.so.2"


def _contract(data):
    def require(condition, message):
        if not condition:
            raise ValueError(message)

    def bounded(offset, size):
        require(0 <= offset <= len(data) and 0 <= size <= len(data) - offset,
                "ELF file range is out of bounds")
        return data[offset:offset + size]

    require(len(data) >= 64 and data[:7] == b"\x7fELF\x02\x01\x01",
            "expected little-endian ELF64")
    fields = struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
    kind, machine, version, _, phoff, shoff, _, ehsize, phsize, phnum, shsize, shnum, shstr = fields
    require(kind in (2, 3) and machine == 62 and version == 1 and ehsize == 64,
            "expected x86-64 executable ELF")
    require(phsize == 56 and 0 < phnum < 65535 and phoff >= 64,
            "invalid ELF program header table")
    bounded(phoff, phsize * phnum)
    if shoff or shnum:
        require(shoff >= 64 and shsize == 64 and shnum > 0 and shstr < shnum,
                "invalid ELF section header table")
        bounded(shoff, shsize * shnum)
        for index in range(shnum):
            section = struct.unpack_from("<IIQQQQIIQQ", data, shoff + index * shsize)
            if section[1] not in (0, 8):  # NULL and NOBITS have no file allocation.
                bounded(section[4], section[5])
    else:
        require(shstr == 0, "stripped ELF names a section string table")
    headers = [struct.unpack_from("<IIQQQQQQ", data, phoff + i * phsize)
               for i in range(phnum)]
    for tag, _, offset, _, _, size, memsize, align in headers:
        bounded(offset, size)
        require(tag != 1 or size <= memsize, "invalid ELF load segment size")
        require(align in (0, 1) or align & (align - 1) == 0, "invalid ELF segment alignment")
    interpreters = [h for h in headers if h[0] == 3]
    dynamics = [h for h in headers if h[0] == 2]
    require(len(interpreters) <= 1 and len(dynamics) <= 1,
            "duplicate ELF interpreter or dynamic segment")
    result = dict(interpreter=None, interpreter_offset=None, interpreter_size=None,
                  needed=[], rpath=None, runpath=None, flags_1=0)
    if interpreters:
        _, _, offset, _, _, size, _, _ = interpreters[0]
        require(offset >= phoff + phnum * phsize and size > 1,
                "invalid ELF interpreter allocation")
        require(not shnum or offset + size <= shoff or shoff + shsize * shnum <= offset,
                "interpreter overlaps ELF section headers")
        raw = bounded(offset, size)
        end = raw.find(b"\0")
        require(end > 0 and not any(raw[end:]), "invalid ELF interpreter termination")
        result.update(interpreter=raw[:end].decode("utf-8", "strict"),
                      interpreter_offset=offset, interpreter_size=size)
        for header in dynamics:
            require(offset + size <= header[2] or header[2] + header[5] <= offset,
                    "interpreter overlaps dynamic segment")
    if not dynamics:
        return result
    dynamic = dynamics[0]
    require(dynamic[5] >= 16 and dynamic[5] % 16 == 0, "invalid ELF dynamic size")
    entries = []
    for offset in range(dynamic[2], dynamic[2] + dynamic[5], 16):
        tag, value = struct.unpack_from("<QQ", data, offset)
        if tag == 0:
            break
        entries.append((tag, value))
    else:
        raise ValueError("unterminated ELF dynamic segment")
    for singleton in (5, 10, 15, 29, 0x6ffffffb):
        require(sum(tag == singleton for tag, _ in entries) <= 1,
                "duplicate ELF dynamic contract tag")
    tags = dict(entries)
    result["flags_1"] = tags.get(0x6ffffffb, 0)
    if any(tag in (1, 15, 29) for tag, _ in entries):
        require(5 in tags and 10 in tags and tags[10] > 0, "missing ELF dynamic strings")
        locations = [h[2] + tags[5] - h[3] for h in headers if h[0] == 1
                     and h[3] <= tags[5] and tags[5] - h[3] <= h[5]
                     and tags[10] <= h[5] - (tags[5] - h[3])]
        require(len(locations) == 1, "unmapped or ambiguous ELF dynamic strings")
        if interpreters:
            start, size = result["interpreter_offset"], result["interpreter_size"]
            require(start + size <= locations[0] or locations[0] + tags[10] <= start,
                    "interpreter overlaps ELF dynamic strings")
        strings = bounded(locations[0], tags[10])
        for tag, value in entries:
            if tag not in (1, 15, 29):
                continue
            require(value < len(strings), "ELF string offset out of bounds")
            end = strings.find(b"\0", value)
            require(end >= value, "unterminated ELF dynamic string")
            text = strings[value:end].decode("utf-8", "strict")
            if tag == 1:
                require(bool(text) and "/" not in text, "non-library ELF dependency")
                result["needed"].append(text)
            else:
                result["rpath" if tag == 15 else "runpath"] = text
    result["needed"].sort()
    return result


def elf_runtime_contract(path):
    return _contract(pathlib.Path(path).read_bytes())


def _verify(contract, build_class, platform):
    if build_class not in ("release", "static"):
        raise ValueError("unknown payload build class")
    if contract["rpath"] is not None or contract["runpath"] is not None or contract["flags_1"] & 0x800:
        raise ValueError("payload retains build-runtime search policy")
    if build_class == "static":
        if contract["interpreter"] is not None or contract["needed"]:
            raise ValueError("static payload has a dynamic runtime")
        return
    if contract["interpreter"] != SUBSTRATE_INTERPRETER:
        raise ValueError("dynamic payload does not use substrate interpreter")
    available = {member.name for member in (pathlib.Path(platform) / "lib").iterdir()
                 if member.is_file() and not member.is_symlink()}
    if any(name not in available for name in contract["needed"]):
        raise ValueError("dynamic payload has an undeclared dependency")


def verify_output_elf(path, build_class, platform):
    _verify(elf_runtime_contract(path), build_class, platform)


def normalize_output_elf(path, build_class, platform):
    """Normalize only a copied final payload, before capture/signing; fail closed."""
    path = pathlib.Path(path)
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
        raise ValueError("normalization requires a private regular copied payload")
    original = path.read_bytes()
    contract = _contract(original)
    normalized = original
    if build_class == "release":
        expected = str(pathlib.Path(platform) / "lib/ld-linux-x86-64.so.2")
        if contract["interpreter"] != expected:
            raise ValueError("unexpected original payload interpreter")
        replacement = SUBSTRATE_INTERPRETER.encode() + b"\0"
        start, size = contract["interpreter_offset"], contract["interpreter_size"]
        if len(replacement) > size:
            raise ValueError("substrate interpreter exceeds allocation")
        normalized = original[:start] + replacement.ljust(size, b"\0") + original[start + size:]
    _verify(_contract(normalized), build_class, platform)
    # Validation precedes all writes. The allocation and every other byte retain
    # their original positions; no segment/section table or build tool is edited.
    if normalized != original:
        # Pin the validated private copy, refusing link substitution before write.
        fd = os.open(path, os.O_WRONLY | os.O_NOFOLLOW)
        try:
            current = os.fstat(fd)
            if (current.st_dev, current.st_ino, current.st_nlink) != (metadata.st_dev, metadata.st_ino, 1):
                raise ValueError("copied payload identity changed before normalization")
            with os.fdopen(fd, "wb", closefd=False) as output:
                output.write(normalized)
        finally:
            os.close(fd)
    return {"before_sha256": hashlib.sha256(original).hexdigest(),
            "after_sha256": hashlib.sha256(normalized).hexdigest(),
            "before_interpreter": contract["interpreter"],
            "after_interpreter": _contract(normalized)["interpreter"]}

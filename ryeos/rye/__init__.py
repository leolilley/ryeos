"""RYE OS - AI operating system on Lillux microkernel."""

from importlib.metadata import PackageNotFoundError, version

try:
    __version__ = version("ryeos-engine")
except PackageNotFoundError:
    __version__ = "0.0.0"

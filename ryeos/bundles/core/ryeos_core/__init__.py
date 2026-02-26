# ryeos-core bundle
from importlib.metadata import version, PackageNotFoundError

try:
    __version__ = version("ryeos-core")
except PackageNotFoundError:
    __version__ = "0.0.0-dev"

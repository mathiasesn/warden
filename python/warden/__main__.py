import os
import sys

from warden import find_warden_bin


def find_warden_bin_and_exec() -> None:
    warden = find_warden_bin()

    if sys.platform == "win32":
        # Windows has no working os.execvp, so spawn and forward the exit
        # code instead. Imported here so the POSIX path, which replaces the
        # process anyway, does not pay for the import on every run.
        import subprocess

        try:
            completed_process = subprocess.run([warden, *sys.argv[1:]])
        except KeyboardInterrupt:
            # 130 (128 + SIGINT), not warden's own usage/I-O error code: an
            # interrupt must not be indistinguishable from a bad invocation.
            # Untested — CI never runs this branch.
            sys.exit(130)
        sys.exit(completed_process.returncode)
    else:
        os.execvp(warden, [warden, *sys.argv[1:]])


if __name__ == "__main__":
    find_warden_bin_and_exec()

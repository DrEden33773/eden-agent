"""Package the installed tree with fast gzip; verification runs against exact bytes."""

import argparse
import pathlib
import tarfile


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=pathlib.Path)
    parser.add_argument(
        "--source", type=pathlib.Path, default=pathlib.Path("artifacts/distribution")
    )
    args = parser.parse_args()
    with tarfile.open(args.archive, "w:gz", compresslevel=1) as bundle:
        bundle.add(args.source, arcname=".")


if __name__ == "__main__":
    main()

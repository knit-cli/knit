"""Exercise the same project setup without declared auth groups."""
import runpy
import sys
from pathlib import Path
sys.argv.append("ungrouped")
runpy.run_path(str(Path(__file__).with_name("auth_groups_pty.py")), run_name="__main__")

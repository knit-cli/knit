# Knit 0.1.0-alpha.55

History queries such as `knit log --all` wait up to 30 seconds, instead of
5, for another query to release the project history lock. The first of two
simultaneous queries may rebuild the history index, which on a busy or cold
disk can take longer than 5 seconds, so the second one failed with "Another
Knit process ... is updating this state".

On Windows, releasing a Knit lock retries deleting the lock file for about a
second. A scanner or indexer briefly holding the new file open could make the
single delete fail and leave the lock held by the still-running process.

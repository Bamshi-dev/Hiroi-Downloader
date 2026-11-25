# Keeping downloads in order

These are features of the background service.

## Queues

Every job is in a queue, `main` unless `hiroi add --queue NAME` says
otherwise. A queue can be held: its running jobs pause, and new ones
wait, until it starts again.

```sh
hiroi queue list
hiroi queue stop night
hiroi queue start night
```

Held queues stay held across restarts.

## Schedules

Rules start queues and limit the speed at set times, by day of the
week.

```sh
hiroi schedule add --queue night --from 01:00 --to 07:00 --days weekdays
hiroi schedule add --limit 1M --from 09:00 --to 18:00
hiroi schedule add --queue backups --from 03:00          # once a day, no end
hiroi schedule list
hiroi schedule remove 2
```

With `--to`, the rule holds a window, which may cross midnight, and
is undone afterwards. Without it, the action happens at `--from` and
stays. `--days` takes `daily`, `weekdays`, `weekends`, or days such as
`mon-fri` or `sat,sun`.

## Categories

Downloads that name no folder are sorted by type into folders of the
download folder: Videos, Music, Pictures, Archives, Documents, and
Programs.

```sh
hiroi category list
hiroi category set ebooks --folder Books --ext epub,mobi
hiroi category set course --folder Course --site uni.example.edu
hiroi category remove ebooks
hiroi category off            # save straight into the download folder
hiroi category on
```

A category for sites takes downloads from those sites and their
subdomains; with `--ext` as well, both have to match. Categories for
sites come before those for types, so a video from the course site goes
to `Course`, not `Videos`.

## Groups

Downloads added together form a group: several URLs given to
`hiroi add`, the choice sent by `hiroi grab --queue`, and a batch from
the browser extension. A group is listed, paused, and resumed as one.

```sh
hiroi add URL1 URL2 URL3 --group "Lecture videos"
hiroi group list
hiroi group pause "Lecture videos"
hiroi group resume "Lecture videos"
```

Without `--group`, the group is named, in this order, after:

1. the series most of its files belong to (see below), such as
   `gardening` for `gardening vid 1.mp4`, `gardening vid 2.mp4`, and
   so on;
2. the words their names begin with, such as `Annual report` for
   `Annual report summary.pdf` and `Annual report tables.xlsx`;
3. the title of the page they came from, without the site's name;
4. the site and the date, such as `example.com 2026-11-20`.

## Smart folders

When a download finishes and an earlier finished download in the same
folder is another episode of the same series, both move into a folder
named after the series, and later episodes follow them there:

```
Videos/gardening vid 1.mp4         Videos/gardening/gardening vid 1.mp4
Videos/Gardening_vid_2.mp4    →    Videos/gardening/Gardening_vid_2.mp4
                                   Videos/gardening/gardening vid 3.mp4 (later)
```

Only files the service downloaded are moved; other files in the folder
are never touched, and nothing is moved over an existing file. The
jobs' paths follow the files. Smart folders are on by default:

```sh
hiroi smart-folders off
hiroi smart-folders on
```

A series is read from the file's name:

- the series' name, then a marker such as `vid`, `video`, `part`,
  `ep`, `episode`, `lecture`, `lesson`, `chapter`, `session`, `week`,
  `day`, `track`, `module`, `disc`, or `vol`, then the number:
  `gardening vid 1`, `Rust Course ep01`, `Rust Course - Part 12`;
- `S01E02`, as TV episodes are named;
- otherwise, the last number of the name: `algebra 7.pdf`.

Numbers that describe the file are skipped: `1080p`, `x264`, `10bit`,
years such as 2024, and numbers of five digits or more. A number
glued to a word belongs to the name (`CS50 lecture 3` is the series
`CS50`). Names cameras and programs number by themselves (`IMG_0001`,
`DSC0042`, `Untitled 2`) are not series, and neither are copies
numbered the way browsers and Hiroi number them (`report (1).pdf`).

Names match regardless of case, spaces, dots, dashes, and
underscores, and in names of six letters or more, a typo is
forgiven: one wrong, missing, extra, or swapped letter (two from
twelve letters). `gardening` and `gardenning` are one series; `math`
and `maps` are not.

## After a download

The service can do more once a download finishes. These are
[settings](settings.md):

- **A command after every download**, such as a virus scanner:
  ```sh
  hiroi settings set after-download 'clamscan --no-summary {path}'
  ```
  `{path}`, `{name}`, and `{dir}` are replaced. The command is split
  like a shell command but runs without a shell, so a file name cannot
  add commands. It runs before a job's own `--then` command.
- **Unpacking archives**, off by default:
  ```sh
  hiroi settings set unpack-archives on
  ```
  A finished zip, 7z, rar, or tar archive is unpacked into a new folder
  next to it, named after it. An archive in parts (`name.part1.rar`,
  `name.7z.001`, ...) waits until every part in the list has finished.
  7-Zip is used when installed, otherwise `unzip`, `unrar`, or `tar`.
  Nothing existing is replaced, and an archive that needs a password
  fails with a notification instead of waiting for one.
- **Staying awake**, on by default: while downloads run, the computer
  does not go to sleep (through `systemd-inhibit` on Linux and
  `caffeinate` on macOS). `hiroi settings set keep-awake off` turns it
  off.

Adding a download that failed again tries it again, in the same job;
other downloads that are in the list already are refused, as
duplicates.

## Moving the list

```sh
hiroi export jobs.json              # the unfinished jobs
hiroi export jobs.json --finished   # all of them
hiroi import jobs.json              # on another machine
```

The list has the addresses, folders, names, queues, groups, and
download options. Headers stay out, since they hold cookies, and so do
commands to run: a list may come from someone else, and importing one
never makes your machine run anything.

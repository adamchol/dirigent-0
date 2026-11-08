use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use dirigent_protocol::Event;

use super::*;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read(root: &Path, path: &str) -> Option<String> {
    fs::read_to_string(root.join(path)).ok()
}

/// A player's side: the workspace, its repository and its files.
struct Player {
    work: tempfile::TempDir,
    _state: tempfile::TempDir,
    repo: Repo,
    files: Workspace,
    last: Option<String>,
}

impl Player {
    async fn new() -> Player {
        let work = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let repo = Repo::open(state.path()).await.unwrap();
        let files = Workspace::new(work.path());
        Player {
            work,
            _state: state,
            repo,
            files,
            last: None,
        }
    }

    fn root(&self) -> &Path {
        self.work.path()
    }

    async fn snapshot(&mut self) -> Option<Snapshot> {
        let s = self
            .repo
            .snapshot(&mut self.files, self.last.as_deref(), "test")
            .await
            .unwrap()?;
        self.last = Some(s.commit.clone());
        Some(s)
    }
}

/// A reader's side: a repository of its own, checking out into `dir`.
struct Reader {
    dir: tempfile::TempDir,
    _state: tempfile::TempDir,
    repo: Repo,
}

impl Reader {
    async fn new() -> Reader {
        let state = tempfile::tempdir().unwrap();
        Reader {
            dir: tempfile::tempdir().unwrap(),
            repo: Repo::open(state.path()).await.unwrap(),
            _state: state,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    async fn apply(&self, s: &Snapshot) -> Vec<PathBuf> {
        let mut bundles = Bundles::default();
        let bundle = events(s)
            .iter()
            .find_map(|e| bundles.push(e))
            .expect("whole");
        self.repo.fetch(&bundle).await.unwrap();
        self.repo
            .checkout(self.root(), &bundle.commit, true)
            .await
            .unwrap()
            .moved
    }
}

#[tokio::test]
async fn a_snapshot_has_what_checkpoints_keep() {
    let mut p = Player::new().await;
    let root = p.root().to_path_buf();
    write(&root, "src/main.rs", "fn main() {}\n");
    write(&root, "scratch.sh", "echo hi\n");
    fs::set_permissions(root.join("scratch.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    write(&root, ".env.example", "A=1\n");
    write(&root, ".gitignore", "target/\n*.log\n");
    write(&root, "target/debug/app", "binary");
    write(&root, "run.log", "noise");
    write(&root, "nested/.gitignore", "cache/\n");
    write(&root, "nested/cache/x", "x");
    // the workspace's own repository, and a repository cloned into it
    write(&root, ".git/HEAD", "ref: refs/heads/main\n");
    write(&root, "vendor/lib/.git/HEAD", "ref: refs/heads/main\n");
    write(&root, "vendor/lib/lib.rs", "pub fn f() {}\n");
    symlink("src/main.rs", root.join("main-link")).unwrap();
    let big = fs::File::create(root.join("big.bin")).unwrap();
    big.set_len(MAX_FILE_SIZE + 1).unwrap();

    let s = p.snapshot().await.unwrap();
    let r = Reader::new().await;
    r.apply(&s).await;
    let mut got = Workspace::new(r.root()).list().await.unwrap();
    got.sort();
    let want: Vec<PathBuf> = [
        ".env.example",
        ".gitignore",
        "main-link",
        "nested/.gitignore",
        "scratch.sh",
        "src/main.rs",
        "vendor/lib/lib.rs",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(got, want);
    let mode = fs::metadata(r.root().join("scratch.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "executable");
    assert_eq!(
        fs::read_link(r.root().join("main-link")).unwrap(),
        PathBuf::from("src/main.rs")
    );
}

#[tokio::test]
async fn an_unchanged_workspace_makes_no_snapshot_and_an_edit_a_small_one() {
    let mut p = Player::new().await;
    let root = p.root().to_path_buf();
    let lines: String = (0..20_000)
        .map(|i| format!("line {i} of a big file\n"))
        .collect();
    write(&root, "big.txt", &lines);
    write(&root, "a.txt", "a\n");
    let first = p.snapshot().await.unwrap();
    assert!(p.snapshot().await.is_none());

    write(
        &root,
        "big.txt",
        &lines.replace("line 7000 ", "line seven thousand "),
    );
    let edit = p.snapshot().await.unwrap();
    assert_eq!(edit.parent.as_deref(), Some(first.commit.as_str()));
    assert!(
        edit.bundle.len() < 2_000,
        "a one-line edit's bundle is {} bytes (the first's {})",
        edit.bundle.len(),
        first.bundle.len()
    );

    let r = Reader::new().await;
    r.apply(&first).await;
    r.apply(&edit).await;
    assert!(read(r.root(), "big.txt")
        .unwrap()
        .contains("line seven thousand "));
}

#[tokio::test]
async fn deletes_and_renames_come_through_and_local_files_are_kept() {
    let mut p = Player::new().await;
    let root = p.root().to_path_buf();
    write(&root, "a.txt", "a\n");
    write(&root, "b.txt", "b\n");
    write(&root, "c.txt", "c\n");
    let first = p.snapshot().await.unwrap();
    let r = Reader::new().await;
    // there before: the same as what comes, and not
    write(r.root(), "a.txt", "a\n");
    write(r.root(), "c.txt", "mine\n");
    let moved = r.apply(&first).await;
    assert_eq!(moved, vec![PathBuf::from("c.txt")]);
    assert_eq!(read(r.root(), "c.txt.local").as_deref(), Some("mine\n"));
    assert_eq!(read(r.root(), "c.txt").as_deref(), Some("c\n"));

    // the developer's own: an edit to what the next commit changes, an edit to what it
    // doesn't, and a build's output
    write(r.root(), "a.txt", "edited\n");
    write(r.root(), "c.txt", "edited too\n");
    write(r.root(), "target/out", "built");
    fs::remove_file(root.join("b.txt")).unwrap();
    fs::create_dir_all(root.join("moved")).unwrap();
    fs::rename(root.join("a.txt"), root.join("moved/a.txt")).unwrap();
    let second = p.snapshot().await.unwrap();
    let moved = r.apply(&second).await;
    assert_eq!(moved, vec![PathBuf::from("a.txt")]);
    assert_eq!(read(r.root(), "a.txt"), None);
    assert_eq!(read(r.root(), "a.txt.local").as_deref(), Some("edited\n"));
    assert_eq!(read(r.root(), "moved/a.txt").as_deref(), Some("a\n"));
    assert_eq!(read(r.root(), "b.txt"), None);
    assert_eq!(read(r.root(), "c.txt").as_deref(), Some("edited too\n"));
    assert_eq!(read(r.root(), "target/out").as_deref(), Some("built"));
}

#[tokio::test]
async fn a_restore_makes_the_workspace_the_last_commit() {
    let mut p = Player::new().await;
    let root = p.root().to_path_buf();
    write(&root, ".gitignore", "target/\n");
    write(&root, "keep.txt", "1\n");
    write(&root, "gone.txt", "1\n");
    let first = p.snapshot().await.unwrap();
    fs::remove_file(root.join("gone.txt")).unwrap();
    write(&root, "keep.txt", "2\n");
    let second = p.snapshot().await.unwrap();

    // a new machine: the workspace as the checkpoint before has it, and a new repository
    let work = tempfile::tempdir().unwrap();
    write(work.path(), ".gitignore", "target/\n");
    write(work.path(), "keep.txt", "1\n");
    write(work.path(), "gone.txt", "1\n");
    write(work.path(), "target/built", "x");
    let state = tempfile::tempdir().unwrap();
    let repo = Repo::open(state.path()).await.unwrap();
    let mut bundles = Bundles::default();
    let all: Vec<Bundle> = [&first, &second]
        .iter()
        .flat_map(|s| events(s))
        .filter_map(|e| bundles.push(&e))
        .collect();
    let (last, err) = repo.replay(all).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(last.as_deref(), Some(second.commit.as_str()));
    repo.checkout(work.path(), &second.commit, false)
        .await
        .unwrap();
    assert_eq!(read(work.path(), "keep.txt").as_deref(), Some("2\n"));
    assert_eq!(read(work.path(), "gone.txt"), None);
    assert_eq!(read(work.path(), "target/built").as_deref(), Some("x"));

    // and the next snapshot builds on it, small
    let mut files = Workspace::new(work.path());
    assert!(repo
        .snapshot(&mut files, last.as_deref(), "t")
        .await
        .unwrap()
        .is_none());
    write(work.path(), "new.txt", "n\n");
    let next = repo
        .snapshot(&mut files, last.as_deref(), "t")
        .await
        .unwrap()
        .unwrap();
    assert!(next.bundle.len() < 1_000, "{} bytes", next.bundle.len());
}

#[test]
fn a_big_bundle_is_split_and_joined_and_a_missing_part_joins_nothing() {
    let bundle: Vec<u8> = (0..(PART * 2)).map(|i| (i % 251) as u8).collect();
    let s = Snapshot {
        commit: "c1".into(),
        parent: Some("c0".into()),
        bundle,
    };
    let evs = events(&s);
    assert_eq!(evs.len(), 3);
    for e in &evs {
        let Event::FilesChanged { bundle, .. } = e else {
            unreachable!()
        };
        assert!(bundle.len() <= PART);
    }
    let mut b = Bundles::default();
    let whole: Vec<Bundle> = evs.iter().rev().filter_map(|e| b.push(e)).collect();
    assert_eq!(
        whole,
        vec![Bundle {
            commit: "c1".into(),
            parent: Some("c0".into()),
            bundle: s.bundle.clone(),
        }]
    );
    let mut b = Bundles::default();
    assert!(evs[..2].iter().all(|e| b.push(e).is_none()));
}

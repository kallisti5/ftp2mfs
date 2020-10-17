use anyhow::Result;
use clap::Clap;
use failure::ResultExt;
use ipfs_api::IpfsClient;
use priority_queue::PriorityQueue;
use rand::prelude::*;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::default::Default;
use std::sync::{Arc, Mutex};
use tokio::signal::unix::{signal, SignalKind};
use tokio::stream::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::{delay_for, delay_until, Duration};

#[derive(Clap, Debug)]
#[clap(about, version)]
struct Opts {
	/// IPFS api url
	#[clap(short, long, default_value = "http://localhost:5001/", env = "IPFS_API")]
	api: String,
	/// Provide files
	#[clap(long, short)]
	files: bool,
	/// Provide folders
	#[clap(long, short = 'd')]
	folders: bool,
	/// Re-resolve root every
	/// (or when hit with SIGUSR1)
	#[clap(long, short = 's', default_value = "15 min")]
	reresolve_root: humantime::Duration,
	/// Reprovide elements every
	#[clap(long, short, default_value = "20 hours")]
	reprovide_interval: humantime::Duration,
	/// Maximum parallel reprovides
	#[clap(long, short = 'j', default_value = "50")]
	parallel_reprovides: usize,
	/// Root path to provide from
	///
	/// Paths starting with /ipfs/ are provided directly,
	/// paths starting with /ipns/ are resolved and provided,
	/// other paths are "resolved" with ipfs files stat.
	#[clap()]
	path: Vec<String>,
}

#[derive(Clone, Debug)]
struct Entry {
	gen: bool,
	sub: Vec<String>,
	nam: String, // Entirely for debugging purposes
}
type FolderTree = HashMap<String, Entry>;
#[derive(Debug, Default)]
struct Folders {
	tree: FolderTree,
	sched: PriorityQueue<String, Reverse<Instant>>,
}

lazy_static::lazy_static! {
	static ref TREE : Mutex<Folders> = Default::default();
	static ref OPTS : Opts = Opts::parse();
	static ref GEN : AsyncMutex<bool> = Default::default(); // Lock so only one walk is active in parallel
	static ref J : Semaphore = Semaphore::new(OPTS.parallel_reprovides);
	static ref CLIENTS : Vec<IpfsClient> = std::iter::repeat(())
			.take(OPTS.parallel_reprovides / 10)
			.map(|()| ipfs_api::TryFromUri::from_str(&OPTS.api).expect("IPFS API URL"))
			.collect();
}

fn client() -> &'static IpfsClient {
	CLIENTS.choose(&mut rand::thread_rng()).unwrap()
}

async fn refresh() -> Result<()> {
	struct Walk {
		s: Semaphore,
		g: bool,
	}
	impl Walk {
		fn jump(s: &str, f: &mut FolderTree, g: bool) {
			let mut c = f.get_mut(s).expect("children must be there");
			if c.gen != g {
				c.gen = g;
				for c in c.sub.clone() {
					Self::jump(&c, f, g);
				}
			}
		}
		#[async_recursion::async_recursion]
		async fn step(self: Arc<Self>, s: String, mut h: Option<JoinHandle<Result<()>>>) -> Result<()> {
			let _ = self.s.acquire().await;
			let links = client()
				.ls(Some(&s))
				.await
				.context(format!("ls {}", s))
				.compat()?
				.objects;
			{
				let mut t = TREE.lock().unwrap();
				let Folders { tree: r, sched: q } = &mut *t;
				let mut sc = r.remove(&s).unwrap();
				for ipfs_api::response::IpfsFile { links, .. } in links {
					for ipfs_api::response::IpfsFileHeader { typ, hash, name, .. } in links.into_iter() {
						if r.contains_key(&hash) {
							Self::jump(&hash, &mut *r, self.g);
						} else {
							if typ == 1 && OPTS.folders || typ != 1 && OPTS.files {
								// As a neat little side effect, a parent folder will always be
								// provided before its children (not necessarily breadth or depth first though)
								q.push(hash.clone(), Reverse(Instant::now()));
								log::info!("Found {} ({})", hash, name);
							}
							if typ == 1 {
								h = Some(tokio::spawn(self.clone().step(hash.clone(), h.take())));
							}
							if typ == 1 || OPTS.files {
								r.insert(
									hash.clone(),
									Entry {
										gen: self.g,
										sub: vec![],
										nam: name,
									},
								);
							}
						}
						if typ == 1 || OPTS.files {
							sc.sub.push(hash.clone());
						}
					}
				}
				r.insert(s, sc);
			}
			if let Some(h) = h {
				h.await?
			} else {
				Ok(())
			}
		}
	}

	let mut s = Vec::new();
	for path in OPTS.path.iter() {
		s.push(if path.starts_with("/ipns/") || path.starts_with("/ipfs/") {
			client()
				.object_stat(path)
				.await
				.context(format!("resolve {}", path))
				.compat()?
				.hash
		} else {
			client()
				.files_stat(path)
				.await
				.context(format!("stat {}", path))
				.compat()?
				.hash
		});
	}
	let s = s;

	let mut sw = Vec::new();
	let mut gen = GEN.lock().await;
	{
		let mut tree = TREE.lock().unwrap();
		use std::collections::hash_map::Entry::*;
		let same = s.iter().all(|s| tree.tree.contains_key(s));
		if same {
			return Ok(());
		} else {
			*gen = !*gen;
			for h in s {
				match tree.tree.entry(h.clone()) {
					Occupied(_) => Walk::jump(&h, &mut tree.tree, *gen),
					Vacant(e) => {
						e.insert(Entry {
							gen: *gen,
							sub: vec![],
							nam: "/".to_owned(),
						});
						tree.sched.push(h.clone(), Reverse(Instant::now()));
						sw.push(h);
					}
				}
			}
		}
	}
	let sw = sw;

	log::info!("Exploring {}", sw.join(", "));
	let w = Arc::new(Walk {
		s: Semaphore::new(20),
		g: *gen,
	});
	let w = sw
		.into_iter()
		.map(|s| tokio::spawn(w.clone().step(s, None)))
		.collect::<Vec<_>>();
	for w in w {
		w.await??;
	}
	TREE.lock().unwrap().tree.retain(|_, e| e.gen == *gen);
	Ok(())
}

async fn refresh_proc() -> Result<()> {
	let mut usr1 = signal(SignalKind::user_defined1())?.timeout(*OPTS.reresolve_root);
	let mut running: Option<JoinHandle<Result<()>>> = Some(tokio::spawn(refresh()));
	loop {
		usr1.try_next().await.ok();
		if let Some(running) = running.take() {
			running.await??;
		}
		running = Some(tokio::spawn(refresh()));
	}
}

async fn refresh_wrapper() {
	loop {
		match refresh_proc().await {
			Ok(()) => unreachable!(),
			Err(e) => {
				log::error!("Searching {}: {:?}", OPTS.path.join(", "), e);
				delay_for(Duration::from_secs(60)).await;
			}
		}
	}
}

#[tokio::main(basic_scheduler)]
async fn main() -> Result<()> {
	pretty_env_logger::init();
	if !OPTS.files && !OPTS.folders {
		anyhow::bail!("Neither --files nor --folders is specified");
	}
	tokio::spawn(refresh_wrapper());
	loop {
		let now = Instant::now();
		let mut next = now + Duration::from_secs(10); // lazy coding: poll if we have no items
		let mut provide = None; // Sync lock, async provide action -.-
		{
			let mut l = TREE.lock().unwrap();
			if let Some((_, Reverse(t))) = l.sched.peek() {
				next = std::cmp::min(next, *t);
				if next < now {
					provide = Some(l.sched.pop().unwrap().0);
				}
			}
		}
		if let Some(provide) = provide.take() {
			let j = J.acquire().await;
			let current = next.clone();
			tokio::spawn(async move {
				use futures_util::stream::TryStreamExt;
				let _ = j;
				let te = TREE.lock().unwrap().tree.get(&provide).map(|e| e.nam.to_owned());
				if let Some(te) = te {
					let lee = Duration::from_secs(300);
					let req = client().dht_provide(&provide).timeout(lee);
					let res = req.try_collect::<Vec<_>>().await.map(|_| ());
					let now = Instant::now();
					let lev = if res.is_ok() && now < next + lee {
						log::Level::Info
					} else if res.is_ok() {
						log::Level::Warn
					} else {
						log::Level::Error
					};
					let mut wait = now - current;
					wait -= Duration::from_nanos(wait.subsec_nanos() as u64);
					log::log!(
						lev,
						"Provided {} ({}) waiting {}: {:?}",
						provide,
						te,
						humantime::Duration::from(wait),
						res
					);
					let next = now
						+ Duration::from(*OPTS.reprovide_interval).mul_f64(rand::thread_rng().gen_range(0.9, 1.));
					TREE.lock().unwrap().sched.push(provide, Reverse(next));
				}
			});
		}
		delay_until(next).await;
	}
}

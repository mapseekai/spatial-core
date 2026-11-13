use anyhow::Result;
use spatial_core::{
    control::Cancellation,
    postgis::{
        PostGisClient, TableName, WriteControl, WriteMode,
        vector::{PostGisVectorWriter, VectorSource},
    },
};
use std::time::{Duration, Instant};
fn main() -> Result<()> {
    let case = std::env::args().nth(1).unwrap_or("hostname".into());
    tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build()?.block_on(async move {
  if case == "tokio_queued_abort" {
   let (release,hold)=std::sync::mpsc::channel();let (started,ready)=tokio::sync::oneshot::channel();
   let unrelated=tokio::task::spawn_blocking(move ||{let _=started.send(());hold.recv().unwrap();});ready.await?;
   let ran=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));let marker=ran.clone();
   let mut queued=tokio::task::spawn_blocking(move ||{marker.store(true,std::sync::atomic::Ordering::SeqCst);});
   queued.abort();let start=Instant::now();let result=tokio::time::timeout(Duration::from_millis(700),&mut queued).await;
   println!("Tokio queued abort before release: {}ms {result:?}, ran={}",start.elapsed().as_millis(),ran.load(std::sync::atomic::Ordering::SeqCst));
   release.send(())?;unrelated.await?;
   if result.is_err(){println!("queued join after release: {:?}",queued.await);}
   println!("after release ran={}",ran.load(std::sync::atomic::Ordering::SeqCst));
   return Ok(());
  }
  let base:tokio_postgres::Config=std::env::var("SC_REVIEW_DSN")?.parse()?;
  let(observer,driver)=base.connect(tokio_postgres::NoTls).await?;tokio::spawn(driver);
  observer.batch_execute("CREATE SCHEMA IF NOT EXISTS review_final_tx").await?;


  if case == "queued_source" {
   let pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(base.clone(),tokio_postgres::NoTls)).max_size(1).build()?;
   pool.get().await?.simple_query("SELECT 1").await?;
   let db=PostGisClient::from_pool(pool.clone(),tokio_postgres::NoTls);
   let (release,hold)=std::sync::mpsc::channel();let (started,ready)=tokio::sync::oneshot::channel();
   let unrelated=tokio::task::spawn_blocking(move || {let _=started.send(());hold.recv().unwrap();});ready.await?;
   let cancel=Cancellation::default();let control=WriteControl::new(cancel.clone());
   let mut write=tokio::spawn(async move {PostGisVectorWriter::new(&db,TableName::new("review_final_tx","queued_source").unwrap()).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&control).await});
   while pool.status().available != 0 {tokio::task::yield_now().await;}
   tokio::task::yield_now().await;cancel.cancel();let start=Instant::now();
   let first=tokio::time::timeout(Duration::from_millis(700),&mut write).await;
   println!("queued source cancellation before unrelated worker released: elapsed={}ms result={first:?}",start.elapsed().as_millis());
   release.send(())?;unrelated.await?;
   if first.is_err(){let done=tokio::time::timeout(Duration::from_secs(3),write).await??;println!("after releasing unrelated worker: {done:?}");}
   return Ok(());
  }
  if case == "commit_reply_loss" {
   use tokio::{net::{TcpListener,TcpStream},io::{AsyncReadExt,AsyncWriteExt}};
   let direct_pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(base.clone(),tokio_postgres::NoTls)).max_size(1).build()?;
   let direct=PostGisClient::from_pool(direct_pool,tokio_postgres::NoTls);
   let table=TableName::new("review_final_tx","commit_reply")?;
   PostGisVectorWriter::new(&direct,table.clone()).mode(WriteMode::ReplaceTable).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&WriteControl::default()).await?;
   observer.batch_execute("UPDATE review_final_tx.commit_reply SET value=999").await?;
   let listener=TcpListener::bind("127.0.0.1:0").await?;let port=listener.local_addr()?.port();let dbport=base.get_ports()[0];
   let proxy=tokio::spawn(async move {
    let(client,_)=listener.accept().await?;let server=TcpStream::connect(("127.0.0.1",dbport)).await?;
    let(mut input,mut output)=client.into_split();let(mut replies,mut requests)=server.into_split();
    let reply=async {
     loop {
      let tag=replies.read_u8().await?;let length=replies.read_u32().await?;let mut body=vec![0;(length-4)as usize];replies.read_exact(&mut body).await?;
      if tag==b'C' && body==b"COMMIT\0"{return Ok::<_,std::io::Error>(());}
      output.write_u8(tag).await?;output.write_u32(length).await?;output.write_all(&body).await?;
     }
    };
    tokio::select! {r=tokio::io::copy(&mut input,&mut requests)=>r.map(|_|()),r=reply=>r}
   });
   let mut cfg=tokio_postgres::Config::new();cfg.host("127.0.0.1").port(port).user(base.get_user().unwrap()).password(base.get_password().unwrap()).dbname(base.get_dbname().unwrap()).ssl_mode(tokio_postgres::config::SslMode::Disable);
   let pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(cfg,tokio_postgres::NoTls)).max_size(1).build()?;let db=PostGisClient::from_pool(pool.clone(),tokio_postgres::NoTls);
   let result=tokio::time::timeout(Duration::from_secs(5),PostGisVectorWriter::new(&db,table).mode(WriteMode::ReplaceRows).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&WriteControl::default())).await?;
   let actual:Vec<i32>=observer.query_one("SELECT array_agg(value ORDER BY value) FROM review_final_tx.commit_reply",&[]).await?.get(0);
   println!("commit ack discarded: result={result:?}, actual={actual:?}, pool_size={}",pool.status().size);
   assert!(matches!(result,Err(spatial_core::postgis::PostGisError::CommitOutcomeUnknown(_))));assert_eq!(actual,vec![1,2,3]);assert_eq!(pool.status().size,0);
   proxy.await??;observer.batch_execute("DROP TABLE review_final_tx.commit_reply").await?;return Ok(());
  }
  if case == "rollback_timeout" {
   use tokio::{net::{TcpListener,TcpStream},io::AsyncReadExt};
   let direct_pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(base.clone(),tokio_postgres::NoTls)).max_size(1).build()?;
   let direct=PostGisClient::from_pool(direct_pool,tokio_postgres::NoTls);
   PostGisVectorWriter::new(&direct,TableName::new("review_final_tx","timeout_target")?).mode(WriteMode::ReplaceTable).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&WriteControl::default()).await?;
   observer.batch_execute("CREATE FUNCTION review_final_tx.sleep_row() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN PERFORM pg_sleep(30); RETURN NEW; END$$;CREATE TRIGGER sleep_row BEFORE INSERT ON review_final_tx.timeout_target FOR EACH ROW EXECUTE FUNCTION review_final_tx.sleep_row()").await?;
   let listener=TcpListener::bind("127.0.0.1:0").await?;let port=listener.local_addr()?.port();let dbport=base.get_ports()[0];
   let proxy=tokio::spawn(async move {
    let(mut client,_)=listener.accept().await?;let mut server=TcpStream::connect(("127.0.0.1",dbport)).await?;
    let mut forward=Box::pin(tokio::io::copy_bidirectional(&mut client,&mut server));
    tokio::select! {
     result=&mut forward=>{result?;},
     next=listener.accept()=>{let(mut cancel,_)=next?;let mut packet=[0u8;16];cancel.read_exact(&mut packet).await?;assert_eq!(u32::from_be_bytes(packet[4..8].try_into().unwrap()),80877102);drop(cancel);forward.await?;},
    }
    Ok::<_,std::io::Error>(())
   });
   let mut cfg=tokio_postgres::Config::new();cfg.host("127.0.0.1").port(port).user(base.get_user().unwrap()).password(base.get_password().unwrap()).dbname(base.get_dbname().unwrap()).ssl_mode(tokio_postgres::config::SslMode::Disable);
   let pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(cfg,tokio_postgres::NoTls)).max_size(1).build()?;
   let pid=pool.get().await?.query_one("SELECT pg_backend_pid()",&[]).await?.get::<_,i32>(0);
   let db=PostGisClient::from_pool(pool.clone(),tokio_postgres::NoTls);let cancel=Cancellation::default();let control=WriteControl::new(cancel.clone());
   let write=tokio::spawn(async move {PostGisVectorWriter::new(&db,TableName::new("review_final_tx","timeout_target").unwrap()).mode(WriteMode::ReplaceRows).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&control).await});
   tokio::time::timeout(Duration::from_secs(5),async {loop{if observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='PgSleep')",&[&pid]).await?.get::<_,bool>(0){return Ok::<_,anyhow::Error>(());}tokio::task::yield_now().await;}}).await??;
   let start=Instant::now();cancel.cancel();let result=tokio::time::timeout(Duration::from_secs(8),write).await;
   observer.query_one("SELECT pg_terminate_backend($1)",&[&pid]).await?;
   println!("dropped cancel packet: elapsed_ms={}, pool_size={}, result={result:?}",start.elapsed().as_millis(),pool.status().size);
   let error=result??.unwrap_err();assert!(matches!(error,spatial_core::postgis::PostGisError::Cleanup{cleanup,..} if matches!(*cleanup,spatial_core::postgis::PostGisError::CleanupTimeout)));
   let count=observer.query_one("SELECT count(*) FROM review_final_tx.timeout_target",&[]).await?.get::<_,i64>(0);assert_eq!(count,3);println!("old row count after backend cleanup={count}");
   observer.batch_execute("DROP TABLE review_final_tx.timeout_target;DROP FUNCTION review_final_tx.sleep_row()").await?;
   proxy.abort();return Ok(());
  }
  if case == "metadata_wait" {
   observer.batch_execute("CREATE TABLE review_final_tx.waiting_meta(old_col integer,geom geometry(Point,4326)); INSERT INTO review_final_tx.waiting_meta VALUES(1,ST_SetSRID(ST_Point(12,55),4326))").await?;
   let pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(base.clone(),tokio_postgres::NoTls)).max_size(1).build()?;
   let pid=pool.get().await?.query_one("SELECT pg_backend_pid()",&[]).await?.get::<_,i32>(0);
   let db=PostGisClient::from_pool(pool,tokio_postgres::NoTls);
   let(ddl,driver)=base.connect(tokio_postgres::NoTls).await?;tokio::spawn(driver);
   ddl.batch_execute("BEGIN;DROP TABLE review_final_tx.waiting_meta;CREATE TABLE review_final_tx.waiting_meta(new_col text,geom geometry(Point,3857));INSERT INTO review_final_tx.waiting_meta VALUES('new',ST_SetSRID(ST_Point(1000,2000),3857))").await?;
   let read=tokio::spawn(async move {spatial_core::postgis::vector::read_metadata(&db,&TableName::new("review_final_tx","waiting_meta").unwrap(),&spatial_core::postgis::ColumnName::new("geom").unwrap(),&WriteControl::default()).await});
   tokio::time::timeout(Duration::from_secs(3),async {loop {if observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND mode='AccessShareLock' AND NOT granted)",&[&pid]).await?.get::<_,bool>(0){return Ok::<_,anyhow::Error>(());} tokio::task::yield_now().await;}}).await??;
   ddl.batch_execute("COMMIT").await?;
   let result=tokio::time::timeout(Duration::from_secs(3),read).await??;
   println!("metadata waiting for committed replacement={result:?}");
   let metadata=result?;assert_eq!(metadata.columns[0].name,"new_col");assert_eq!(metadata.srid,Some(3857));assert_eq!(metadata.feature_count,1);assert_eq!(metadata.extent,Some([1000.,2000.,1000.,2000.]));
   observer.batch_execute("DROP TABLE review_final_tx.waiting_meta").await?;
   return Ok(());
  }
  if case == "event_trigger" {
   observer.batch_execute("CREATE OR REPLACE FUNCTION review_final_tx.after_index() RETURNS event_trigger LANGUAGE plpgsql AS $$BEGIN IF EXISTS(SELECT 1 FROM pg_event_trigger_ddl_commands() d JOIN pg_index i ON i.indexrelid=d.objid WHERE i.indrelid=to_regclass('review_final_tx.event_target')) THEN INSERT INTO review_final_tx.event_target(name,value,geom) VALUES('extra',99,ST_SetSRID(ST_MakePoint(99,50),4326)); END IF; END$$; CREATE EVENT TRIGGER sc_review_final_tx_after_index ON ddl_command_end WHEN TAG IN ('CREATE INDEX') EXECUTE FUNCTION review_final_tx.after_index()").await?;
   let pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(base.clone(),tokio_postgres::NoTls)).max_size(1).build()?;
   let db=PostGisClient::from_pool(pool,tokio_postgres::NoTls);
   let result=PostGisVectorWriter::new(&db,TableName::new("review_final_tx","event_target")?).mode(WriteMode::ReplaceTable).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&WriteControl::default()).await;
   let actual=observer.query_one("SELECT count(*),ST_Extent(geom)::text FROM review_final_tx.event_target",&[]).await;
   println!("event trigger result={result:?}");println!("actual={:?}",actual.map(|r|(r.get::<_,i64>(0),r.get::<_,Option<String>>(1))));
   observer.batch_execute("DROP EVENT TRIGGER sc_review_final_tx_after_index;DROP FUNCTION review_final_tx.after_index();DROP TABLE IF EXISTS review_final_tx.event_target").await?;
   return Ok(());
  }

  let mut config=tokio_postgres::Config::new();config.host(if case=="ip"{"127.0.0.1"}else{"localhost"}).port(base.get_ports()[0]).user(base.get_user().unwrap()).password(base.get_password().unwrap()).dbname(base.get_dbname().unwrap()).ssl_mode(tokio_postgres::config::SslMode::Disable);
  let pool=deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(config,tokio_postgres::NoTls)).max_size(1).build()?;
  if case=="warm"{pool.get().await?.simple_query("SELECT 1").await?;}
  let db=PostGisClient::from_pool(pool,tokio_postgres::NoTls);
  let cancel=Cancellation::default();let control=WriteControl::new(cancel.clone());let start=Instant::now();
  let name=format!("cold_{case}");
  let mut write=Box::pin(PostGisVectorWriter::new(&db,TableName::new("review_final_tx",&name)?).mode(WriteMode::ReplaceTable).write(VectorSource::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../testdata/points.geojson")),&control));
  match tokio::time::timeout(Duration::from_secs(3),&mut write).await {
    Ok(result)=>println!("{case}: completed {}ms: {result:?}",start.elapsed().as_millis()),
    Err(_)=>{println!("{case}: STILL BLOCKED after {}ms",start.elapsed().as_millis());cancel.cancel();let result=tokio::time::timeout(Duration::from_secs(5),write).await?;println!("cancel={result:?}");}
  }
  observer.batch_execute(&format!("DROP TABLE IF EXISTS review_final_tx.{name}")).await?;
  Ok(())
 })
}

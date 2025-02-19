use std::time::Duration;
use ratel::*;

use std::fs;
use std::io::Write;
use std::thread;
use std::sync::{mpsc,Arc};
use std::sync::mpsc::{Receiver, SyncSender};
use std::collections::HashMap;

use std::io::stdout;
use std::io::BufWriter;

use async_std::task::block_on;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use dns_lookup::lookup_host;
use cidr_utils::cidr::IpCidr;
use num_bigint::ToBigUint;

use umya_spreadsheet::{self, Style};
use umya_spreadsheet::{Font,Color};

use ratel::RecordType::Other;

//mod http_banner;
//注意了，需要在crate root 引入mod，这样在finger子mod下面才能use super::http_finger::socket_get;其它mode的内容

//mod detect_mod;
//use detect_mod::Detector;

//mod active;
//use active::{Host,TargetIter,ActiveRecordIter,scan_port};

//mod passive;
//use passive::*;

/*
impl Message {
    fn display(&self) {
        match self {
            Message::Content(r) => {
                println!("Message: {:?} is {} record",r.record(),{if r.is_active() { "active"} else if r.is_other() { "other" } else { "passive" } });
            },
            Message::Finished => {
                println!("Message is Finished");
            }
        }
    }
}
*/

fn passive(conf:PassiveConfig,result_sender:SyncSender<Message>) {
    if conf.searchs.len() == 0 {
        println!("[!] Passive search got nothing input");
        return;
    };
    let fofa_email = conf.fofa_email;
    let fofa_key = conf.fofa_key;
    let fofa_timeout = Duration::from_secs(conf.fofa_timeout);
    let fofa_retry_delay = Duration::from_secs(conf.fofa_retry_delay as u64);
    let fofa_delay = Duration::from_secs(conf.fofa_delay as u64);

    let zoomeye_keys = conf.zoomeye_keys;
    let zoomeye_timeout = Duration::from_secs(conf.zoomeye_timeout);
    let zoomeye_delay = Duration::from_secs(conf.zoomeye_delay as u64);

    let es = read_excludes(conf.exclude_files);
    let mut searchs = Vec::new();
    for s in conf.searchs.iter() {
        if !es.contains(s) {
            searchs.push(s.clone());
        }
    }
    drop(conf.searchs);
    drop(es);
    let searchs1 = Arc::new(searchs);
    let searchs2 = Arc::clone(&searchs1);

    let fofa_sender = result_sender.clone();
    let zoomeye_sender = result_sender.clone();

    let mut handlers = Vec::new();
    if conf.fofa_enable {
        if fofa_email != "" && fofa_key != "" {
            handlers.push(Some(thread::spawn(move||{
                fofa_search(conf.run_mod,searchs1,fofa_sender,fofa_email, fofa_key, conf.fofa_per_page_size, fofa_timeout,conf.passive_retries,fofa_retry_delay,fofa_delay,conf.auto_web_filter);
            })));
        } else {
            println!("[-] Fofa auth info not set... pass fofa query");
        }
    } else {
        println!("[-] Fofa disabled");
    }
    
    if conf.zoomeye_enable {
        if zoomeye_keys.len() > 0 {
            handlers.push(Some(thread::spawn(move||{
                zoomeye_search(conf.run_mod,searchs2,zoomeye_sender,zoomeye_keys,zoomeye_timeout,conf.passive_retries,zoomeye_delay,conf.auto_web_filter);
                //zoomeye_sender.send(Message::Content(Box::new(OtherRecord::new("zoomeye finished".to_string())))).unwrap();
            })));
        } else {
            println!("[-] Zoomeye keys not set... pass zoomeye query");
        }
    } else {
        println!("[-] Zoomeye disabled");
    }
    for h in handlers.iter_mut() {
        h.take().unwrap().join().unwrap();
    }
    result_sender.send(Message::Finished).unwrap();   
}

async fn active(conf:ActiveConfig,result_sender:SyncSender<Message>) {
    println!("=> connect retries:      {}",conf.conn_retries);
    println!("=> async scan limit:     {}",conf.async_scan_limit);
    let mut target_iter = TargetIter::new();
    let mut known_hosts = Vec::new();           //已知主机，只包含domain和ip，cidr不会包含
    let mut open_hosts = Vec::new();          //有开放端口的主机
    if conf.targets.len() == 0 {
        println!("[!] Active port scan got nothing input");
        return;
    };
    let es = read_excludes(conf.exclude_files);
    for t in conf.targets.iter() {
        if es.contains(t) {
            continue;
        }
        if t.contains("/") {
            if let Ok(cidr) = IpCidr::from_str(t) {
                target_iter.append_cidr(cidr);
            } else {
                let info = t.to_string();
                println!("[-] Unknown host {}",t);
                result_sender.send(Message::Content(Box::new(
                    OtherRecord::new(OtherRecordInfo::UnknownHost(info))
                ))).unwrap();
            }
        } else {
            match lookup_host(t) {
                Ok(h) => {
                    let host = Host{
                        host: t.to_string(),
                        ip:h[0].to_string()
                    };
                    known_hosts.push(host.clone());
                    target_iter.append(host);
                },
                Err(_) => {
                    let info = t.to_string();
                    println!("[-] Unknown host {}",t);
                    result_sender.send(Message::Content(Box::new(
                        OtherRecord::new(OtherRecordInfo::UnknownHost(info))
                    ))).unwrap();
                }
            }
        }
    }
    drop(conf.targets);
    drop(es);
    let targets_num = target_iter.total();
    let ports_num = &conf.scan_ports.len().to_biguint().unwrap();
    println!("[-] Active scan: hosts:{} ports:{} total:{}",targets_num,ports_num,targets_num*ports_num);
    let mut active_record_iter = ActiveRecordIter::new(target_iter,conf.scan_ports);
    let mut ftrs = FuturesUnordered::new();
    for _ in 0..conf.async_scan_limit {
        if let Some(record) = active_record_iter.next() {
            ftrs.push(scan_port(record,Duration::from_millis(conf.conn_timeout),conf.conn_retries));
        }
    }
    while let Some(record) = ftrs.next().await {
        if let Some(r) = record {
            if !open_hosts.contains(&r.host().to_string()) {
                //如果扫描的的cidr地址，端口开放主机数量很多的话，open_hosts相关代码会产生大量内存开销！
                open_hosts.push(r.host().to_string());
            }
            result_sender.send(Message::Content(Box::new(r))).unwrap();
        }
        if let Some(r ) = active_record_iter.next() {
            ftrs.push(scan_port(r ,Duration::from_millis(conf.conn_timeout),conf.conn_retries));
        }
    }
    //无端口开放主机发送到结果处理
    for h in known_hosts.iter() {
        if !open_hosts.contains(&h.host) {
            result_sender.send(Message::Content(Box::new(
                OtherRecord::new(OtherRecordInfo::NoOpenPort(format!("{} {}",h.host,h.ip)))
            ))).unwrap();
        }
    }
    result_sender.send(Message::Finished).unwrap();
}

fn urls_finger(conf:UrlsConfig,result_sender:SyncSender<Message>) {
    if conf.urls.len() == 0 {
        println!("[!] Url scan got nothing input");
        return;
    };
    println!("[-] Url scan: total:{}",conf.urls.len());
    let es = read_excludes(conf.exclude_files);
    for u in conf.urls.iter() {
        if !es.contains(u) {
            if let Some(record) = url_to_passive_record(u) {
                result_sender.send(Message::Content(Box::new(record))).unwrap();
            };
        }
    }
    result_sender.send(Message::Finished).unwrap();
}

fn is_assets(rst_config:&ResultConfig,data:&Data) -> bool {
    for domain in rst_config.it_assets.0.iter() {
        if domain.contains(&data.host) || data.host.contains(domain) {
            return true;
        }
        for c_d in data.cert_domains.iter() {
            if c_d.contains(domain) {
                return true;
            } 
        }
    }
    for ip in rst_config.it_assets.1.iter() {
        if ip.starts_with(&data.ip) || ip.starts_with(&data.host) {
            return true;
        } 
    }
    return false;
}

use calamine::{Reader,Xlsx, open_workbook};

async fn result_handler(rst_config:ResultConfig,receiver:Receiver<Message>) {
    let detector = Detector::new(&rst_config);
    let mut results:Vec<Data> = Vec::new();
    let mut caches:HashMap<String,NoNeedCheckDataCache> = HashMap::new();  //{"host:port":cache} 最后和results合并
    let mut checked = String::with_capacity(1024*10);      //记录已经check的host:port
    let mut excludes = String::with_capacity(1024*20);     //记录排除的host:port
    for ef in  rst_config.poc_exclude_files.iter() {
        let excel: Result<Xlsx<_>,_> = open_workbook(ef);
        match excel {
            Ok(mut sheet1) => {
                if let Some(Ok(r)) = sheet1.worksheet_range("Sheet1") {
                    for row in r.rows() {
                        if row.len() >= 4 {
                            //println!("{:?}",row);
                            if let Some(host) = row[1].get_string() {
                                if let Some(port) = row[3].get_float() {
                                    if let Some(protocol) = row[4].get_string() {
                                        let mut host_port = format!("{}:{}",host,port as u16);
                                        //println!("{}",host_port);
                                        if protocol == "http" || protocol == "https" {   //有的端口http https都可以访问，所以这里需要加上协议，否则可能跳过一些链接！
                                            host_port = format!("{}{} ",row[4],host_port)
                                        }
                                        excludes.push_str(&host_port);
                                    }
                                }
                            }
                        }
                    }
                }
            },
            Err(_) => {
                println!("[!] Read exclude file {} error.",ef);
            }
        };
    };
    let mut ftrs  = FuturesUnordered::new();
    let notice_path = &format!("{}_results_notice.txt",rst_config.output_file_name);
    let mut other_f:BufWriter<Box<dyn Write>> = match fs::OpenOptions::new().create(true).write(true) .open(notice_path) {
        Ok(f)=> {
            BufWriter::new(Box::new(f))
        },
        Err(_) => {
            println!("[!] Can not open result notice file {}! notice will be printed to stdout.",notice_path);
            BufWriter::new(Box::new(stdout()))
        }
    };
    let mut ftrs_num = 0;
    let mut recv_finished = false;
    loop {
        if ftrs_num == rst_config.detect_limit {
            break;
        }
        if recv_finished {
            break;
        }
        if let Ok(m) = receiver.recv() {
            match m {
                Message::Content(mut record) => {
                    //去重处理！///////////////////////////////////////////////////////////////////////////////// 保留title
                    if record.record_type() == Other {
                        let info = [record.record().unwrap().as_bytes(),b"\n"].concat();
                        other_f.write(&info).unwrap_or_else(|_|{0});
                        //把其它记录保存到其它结果中/////////////////////////
                    } else { //active passive
                        let mut host_port = record.record().unwrap();
                        let protocol = record.protocol();
                        if protocol == "http" || protocol == "https" {   //有的端口http https都可以访问，所以这里需要加上协议，否则可能跳过一些链接！
                            host_port = format!("{}{}",protocol,host_port)
                        }
                        if excludes.contains(&host_port) {
                            //let info = format!("in excludes: {} title?{}\n",host_port,record.title());
                            //other_f.write(info.as_bytes()).unwrap_or_else(|_|{0});
                            continue;
                        } else {
                            if checked.contains(&host_port) {
                                let title = record.title().trim().to_string();
                                let mut cert_domains = record.cert_domains().unwrap_or_else(||{vec![]});
                                if let Some(cache) = caches.get_mut(&host_port) {
                                    if cache.title != title {
                                        cache.title += &title;
                                    }
                                    for _ in 0..cert_domains.len() {
                                        let domain = cert_domains.pop().unwrap();
                                        if !cache.cert_domains.contains(&domain) {
                                            cache.cert_domains.push(domain);
                                        }
                                    }
                                } else {
                                    caches.insert(host_port,NoNeedCheckDataCache{title,cert_domains});
                                }
                            } else {
                                checked.push_str(&(host_port+" "));
                                ftrs.push(detector.detect(record));
                                ftrs_num += 1;
                            }
                        }
                    }
                },
                Message::Finished => {
                    recv_finished = true;
                    break;
                }
            }
        } else {
            break;
        }
    }
    while let Some(data) = ftrs.next().await {  //注意了 等待一个future完成，则接收一个record，如果这个record没有push到futures中！，那么实际上futures会比record少，这时futures执行完了，record可能还没发送完，就会出现bug!
        if let Some(mut data) = data {
            if data.level >= rst_config.print_level && data.status_code > 0 {
                println!("[+] {}://{}:{} [{}] [{}] {:?}",data.protocol,data.host,data.port,data.title,data.status_code,data.infos);
            }
            data.is_assets = is_assets(&rst_config,&data);
            results.push(data);
        }
        if !recv_finished {
            if let Ok(m) = receiver.recv() {
                match m {
                    Message::Content(mut record) => {
                        if record.record_type() == Other {
                            let info = [record.record().unwrap().as_bytes(),b"\n"].concat();
                            other_f.write(&info).unwrap_or_else(|_|{0});
                            //把其它记录保存到其它结果中/////////////////////////
                        } else {
                            let mut host_port = record.record().unwrap();
                            let protocol = record.protocol();
                            if protocol == "http" || protocol == "https" {   //有的端口http https都可以访问，所以这里需要加上协议，否则可能跳过一些链接！
                                host_port = format!("{}{}",protocol,host_port)
                            }
                            if excludes.contains(&host_port) {
                                //let info = format!("in excludes: {} title?{}\n",host_port,record.title());
                                //other_f.write(info.as_bytes()).unwrap_or_else(|_|{0});
                                ftrs.push(detector.detect(Box::new(OtherRecord::new(OtherRecordInfo::Padding))));  //需要push一个future 平衡futures和record的数量
                            } else {
                                if checked.contains(&host_port) {
                                    let title = record.title().trim().to_string();
                                    let mut cert_domains = record.cert_domains().unwrap_or_else(||{vec![]});
                                    if let Some(cache) = caches.get_mut(&host_port) {
                                        if cache.title != title {
                                            cache.title += &title;
                                        }
                                        for _ in 0..cert_domains.len() {
                                            let domain = cert_domains.pop().unwrap();
                                            if !cache.cert_domains.contains(&domain) {
                                                cache.cert_domains.push(domain);
                                            }
                                        }
                                    } else {
                                        caches.insert(host_port,NoNeedCheckDataCache{title,cert_domains});
                                    }
                                    ftrs.push(detector.detect(Box::new(OtherRecord::new(OtherRecordInfo::Padding))));  //需要push一个future 平衡futures和record的数量
                                } else {
                                    checked.push_str(&(host_port+" "));
                                    ftrs.push(detector.detect(record));
                                }
                            }
                        }
                    },
                    Message::Finished => {
                        recv_finished = true;
                        //println!("recv finished after limit"); //////////
                        //break;  //这里不能break，record接收完了，还需要等待futures完成
                    }
                }
            } else {
                recv_finished = true;
            }
        }
    }
    //println!("{:?}",caches);
    for data in results.iter_mut() {
        let host_port = format!("{}:{}",data.host,data.port);
        if let Some(cache) = caches.get_mut(&host_port) {
            if data.title != cache.title {
                data.title += &cache.title;
            }
            for _ in 0..cache.cert_domains.len() {
                let d = cache.cert_domains.pop().unwrap();
                data.cert_domains.push(d);
            }
        }
    }
    //结果保存到文件
    if results.len() > 0 {
        let result_path = &format!("{}_results.xlsx",rst_config.output_file_name);
        println!("[-] Result saved in \"{}\",total {}.",result_path,results.len());
        let mut book = umya_spreadsheet::new_file();
        let sheet1 = book.get_sheet_mut(0);
        //sheet1.set_title("Ratel Results");  //默认sheet1的名字就是Sheet1，因为这个crate在set_title时没有检查title，而是先检查title再set_title...
        let mut blue_color = Color::default();
        blue_color.set_argb(Color::COLOR_BLUE);
        let mut column_names_font = Font::default();
        column_names_font.set_color(blue_color);
        let mut style = Style::default();
        style.set_font(column_names_font);
        sheet1.set_style_by_range("A1:K1", style);
        let column_names = ["title","host","ip","port","protocol","url","infos","status","cert_domains","is_assets","level"];
        for i in 0..column_names.len() {
            sheet1.get_cell_by_column_and_row_mut((i+1) as u32,1).set_value(column_names[i]);
        }
        for i in 0..results.len() {
            let cols = data_to_list(&results[i]);
            let row = (i+2) as u32;
            if results[i].is_assets {
                let mut blue_color = Color::default();
                blue_color.set_argb(Color::COLOR_DARKGREEN);
                let mut column_names_font = Font::default();
                column_names_font.set_color(blue_color);
                let mut style = Style::default();
                style.set_font(column_names_font);
                sheet1.set_style_by_range(&format!("A{}:K{}",row,row), style);
            }
            for c in 0..cols.len() {
                let col = (c+1) as u32;
                match &cols[c] {
                    &CellType::Str(s) => {
                        sheet1.get_cell_by_column_and_row_mut(col,row).set_value(s);
                    },
                    CellType::Strin(s) => {
                        sheet1.get_cell_by_column_and_row_mut(col,row).set_value(s);
                    },
                    &CellType::Num(n) => {
                        sheet1.get_cell_by_column_and_row_mut(col,row).set_value_from_u16(n);
                    },
                    &CellType::Boolean(b) => {
                        sheet1.get_cell_by_column_and_row_mut(col,row).set_value_from_bool(b);
                    }
                }
            }
        }
        let path = std::path::Path::new(&result_path);
        umya_spreadsheet::writer::xlsx::write(&book, path).unwrap_or_else(|_|{
            println!("[!] Can not save results to file {}! results will be printed to stdout.",result_path);
            for d in results.iter() {
                println!("{:?}\n",d);
            }
        });
    } else {
        println!("[!] No results found");
    }
}

static  BANNER:&'static str = r"______________________________________________________
  ______     ______     ______   ______     __        
 /\  == \   /\  __ \   /\__  _\ /\  ___\   /\ \       
 \ \  __<   \ \  __ \  \/_/\ \/ \ \  __\   \ \ \____  
  \ \_\ \_\  \ \_\ \_\    \ \_\  \ \_____\  \ \_____\ 
   \/_/ /_/   \/_/\/_/     \/_/   \/_____/   \/_____/ 
_____________________________________Author: h4cnull__
";
fn main() {
    println!("{}",BANNER);
    let (rst_config,conf) = get_config();
    println!("=> connection timeout:   {}",rst_config.conn_timeout);
    println!("=> write timeout:        {}",rst_config.write_timeout);
    println!("=> read timeout:         {}",rst_config.read_timeout);
    println!("=> poc enabled:          {}",!rst_config.disable_poc);
    println!("=> pocs file:            {}",rst_config.pocs_file);
    println!("=> redirect_times:       {}",rst_config.redirect_times);
    println!("=> url requests limit:   {}",rst_config.poc_limit);
    println!("=> output name:          {}",rst_config.output_file_name);
    //println!("=> ip:port detect limit: {}",rst_config.detect_limit);
    let (result_sender,result_receiver) = mpsc::sync_channel::<Message>(rst_config.poc_limit as usize);
    let mut handlers = Vec::new();
    handlers.push(Some(thread::spawn(move||{
        block_on(result_handler(rst_config, result_receiver));
    })));
    match conf {
        Config::Passive(p) => {
            handlers.push(Some(thread::spawn(move||{
                passive(p,result_sender)}
            )));
        },
        Config::Active(a) => {
            handlers.push(Some(thread::spawn(move||{
                block_on(active(a,result_sender));
            })));
        },
        Config::Urls(u) => {
            handlers.push(Some(thread::spawn(move||{
                urls_finger(u,result_sender);
            })));
        }
    } 
    for h in handlers.iter_mut() {
        h.take().unwrap().join().unwrap();
    }
}

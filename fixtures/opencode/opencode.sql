
create table `session` (`id` text primary key, `project_id` text, `parent_id` text, `slug` text, `directory` text, `title` text, `version` text, `time_created` integer, `time_updated` integer);
create table `message` (`id` text primary key, `session_id` text, `time_created` integer, `time_updated` integer, `data` text);
create table `part` (`id` text primary key, `message_id` text, `session_id` text, `time_created` integer, `time_updated` integer, `data` text);
create table `todo` (`session_id` text, `content` text);
insert into session values('ses_5c1d3e5f7ffeNn24BbVvCcXxZz','global',NULL,'quiet-owl','/home/demo/db-proj','DB session','1.2.0',1777900000000,1777900009000);
insert into session values('ses_5c1d3e5f8ffeNn24BbVvCcXxZz','global','ses_5c1d3e5f7ffeNn24BbVvCcXxZz','sub','/home/demo/db-proj','Subagent','1.2.0',1777900005000,1777900008000);
insert into message values('msg_5c1d3e5f7001AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900001000,1777900001000,'{"role":"user","time":{"created":1777900001000},"agent":"build","model":{"providerID":"anthropic","modelID":"claude-sonnet-4-5"}}');
insert into message values('msg_5c1d3e5f7002AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900002000,1777900003000,'{"role":"assistant","parentID":"msg_5c1d3e5f7001AaSsDdFfGgHhJj","time":{"created":1777900002000},"modelID":"claude-sonnet-4-5","providerID":"anthropic","agent":"build","finish":"stop"}');
insert into message values('msg_5c1d3e5f7003AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900004000,1777900004000,'not json at all');
insert into message values('msg_5c1d3e5f8001AaSsDdFfGgHhJj','ses_5c1d3e5f8ffeNn24BbVvCcXxZz',1777900006000,1777900006000,'{"role":"user","time":{"created":1777900006000},"agent":"explore","model":{"providerID":"anthropic","modelID":"claude-haiku-4-5"}}');
insert into part values('prt_5c1d3e5f7001AaSsDdFfGgHhJj','msg_5c1d3e5f7001AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900001000,1777900001000,'{"type":"text","text":"read the readme"}');
insert into part values('prt_5c1d3e5f7002AaSsDdFfGgHhJj','msg_5c1d3e5f7002AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900002000,1777900002000,'{"type":"tool","callID":"call_db_1","tool":"read","state":{"status":"completed","input":{"filePath":"README.md"},"output":"# Demo","time":{"start":1777900002500,"end":1777900002600}}}');
insert into part values('prt_5c1d3e5f7003AaSsDdFfGgHhJj','msg_5c1d3e5f7002AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900003000,1777900003000,'{"type":"text","text":"It is a demo."}');
insert into part values('prt_5c1d3e5f7004AaSsDdFfGgHhJj','msg_5c1d3e5f7002AaSsDdFfGgHhJj','ses_5c1d3e5f7ffeNn24BbVvCcXxZz',1777900003500,1777900003500,'oops');
insert into part values('prt_5c1d3e5f8001AaSsDdFfGgHhJj','msg_5c1d3e5f8001AaSsDdFfGgHhJj','ses_5c1d3e5f8ffeNn24BbVvCcXxZz',1777900006000,1777900006000,'{"type":"text","text":"explore the repo"}');

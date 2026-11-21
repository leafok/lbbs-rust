
/*!40101 SET @OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT */;
/*!40101 SET @OLD_CHARACTER_SET_RESULTS=@@CHARACTER_SET_RESULTS */;
/*!40101 SET @OLD_COLLATION_CONNECTION=@@COLLATION_CONNECTION */;
/*!50503 SET NAMES utf8mb4 */;
/*!40103 SET @OLD_TIME_ZONE=@@TIME_ZONE */;
/*!40103 SET TIME_ZONE='+00:00' */;
/*!40014 SET @OLD_UNIQUE_CHECKS=@@UNIQUE_CHECKS, UNIQUE_CHECKS=0 */;
/*!40014 SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0 */;
/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='NO_AUTO_VALUE_ON_ZERO' */;
/*!40111 SET @OLD_SQL_NOTES=@@SQL_NOTES, SQL_NOTES=0 */;
mysqldump: Error: 'Access denied; you need (at least one of) the PROCESS privilege(s) for this operation' when trying to dump tablespaces
DROP TABLE IF EXISTS `admin_config`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `admin_config` (
  `AID` smallint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `begin_dt` datetime DEFAULT NULL,
  `end_dt` datetime DEFAULT NULL,
  `enable` tinyint unsigned NOT NULL DEFAULT '1',
  `major` tinyint unsigned NOT NULL DEFAULT '0',
  PRIMARY KEY (`AID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=4 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `article_favorite`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `article_favorite` (
  `UID` mediumint NOT NULL,
  `AID` int NOT NULL,
  PRIMARY KEY (`UID`,`AID`),
  KEY `AID` (`AID`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `ban_user_list`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `ban_user_list` (
  `BID` smallint NOT NULL AUTO_INCREMENT,
  `SID` smallint NOT NULL DEFAULT '0',
  `UID` mediumint NOT NULL DEFAULT '0',
  `day` smallint NOT NULL DEFAULT '0',
  `ban_UID` mediumint NOT NULL DEFAULT '0',
  `ban_dt` datetime DEFAULT NULL,
  `ban_ip` varchar(20) DEFAULT NULL,
  `unban_UID` mediumint NOT NULL DEFAULT '0',
  `unban_dt` datetime DEFAULT NULL,
  `unban_ip` varchar(20) DEFAULT NULL,
  `enable` tinyint unsigned NOT NULL DEFAULT '1',
  `reason` mediumtext,
  PRIMARY KEY (`BID`),
  KEY `ban_UID` (`ban_UID`),
  KEY `SID` (`SID`),
  KEY `day` (`day`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=483 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `bbs`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `bbs` (
  `AID` int NOT NULL AUTO_INCREMENT,
  `SID` smallint NOT NULL DEFAULT '0',
  `TID` mediumint NOT NULL DEFAULT '0',
  `UID` mediumint NOT NULL DEFAULT '0',
  `username` varchar(20) DEFAULT NULL,
  `nickname` varchar(20) DEFAULT NULL,
  `title` varchar(80) DEFAULT NULL,
  `CID` mediumint NOT NULL DEFAULT '0',
  `sub_dt` datetime DEFAULT NULL,
  `sub_ip` varchar(20) DEFAULT NULL,
  `reply_note` tinyint unsigned NOT NULL DEFAULT '0',
  `visible` tinyint unsigned NOT NULL DEFAULT '1',
  `view_count` smallint NOT NULL DEFAULT '0',
  `exp` mediumint NOT NULL DEFAULT '0',
  `last_reply_dt` datetime DEFAULT NULL,
  `last_reply_UID` mediumint NOT NULL DEFAULT '0',
  `last_reply_username` varchar(20) DEFAULT NULL,
  `last_reply_nickname` varchar(20) DEFAULT NULL,
  `transship` tinyint unsigned NOT NULL DEFAULT '0',
  `lock` tinyint unsigned NOT NULL DEFAULT '0',
  `reply_count` smallint NOT NULL DEFAULT '0',
  `icon` smallint DEFAULT NULL,
  `length` mediumint NOT NULL DEFAULT '0',
  `excerption` tinyint unsigned NOT NULL DEFAULT '0',
  `ontop` tinyint unsigned NOT NULL DEFAULT '0',
  `static` tinyint unsigned NOT NULL DEFAULT '0',
  `gen_ex` tinyint unsigned NOT NULL DEFAULT '0',
  `m_del` tinyint unsigned NOT NULL DEFAULT '0',
  `old_SID` smallint NOT NULL DEFAULT '0',
  PRIMARY KEY (`AID`),
  UNIQUE KEY `CID` (`CID`),
  KEY `UID` (`UID`),
  KEY `reply_count` (`reply_count`),
  KEY `last_reply_dt` (`last_reply_dt`),
  KEY `view_count` (`view_count`),
  KEY `sub_dt` (`sub_dt`),
  KEY `title` (`title`(10)),
  KEY `old_SID` (`old_SID`),
  KEY `SID` (`SID`),
  KEY `TID` (`TID`),
  KEY `last_reply_UID` (`last_reply_UID`)
) ENGINE=InnoDB AUTO_INCREMENT=472401 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `bbs_article_op`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `bbs_article_op` (
  `MID` int NOT NULL AUTO_INCREMENT,
  `AID` int NOT NULL DEFAULT '0',
  `UID` mediumint NOT NULL DEFAULT '0',
  `type` char(1) NOT NULL DEFAULT '',
  `op_dt` datetime DEFAULT NULL,
  `op_ip` varchar(20) DEFAULT NULL,
  `complete` tinyint unsigned NOT NULL DEFAULT '1',
  PRIMARY KEY (`MID`),
  KEY `AID` (`AID`),
  KEY `UID` (`UID`),
  KEY `type` (`type`)
) ENGINE=InnoDB AUTO_INCREMENT=543605 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `bbs_content`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `bbs_content` (
  `CID` int NOT NULL AUTO_INCREMENT,
  `AID` int NOT NULL DEFAULT '0',
  `content` longtext,
  PRIMARY KEY (`CID`),
  KEY `AID` (`AID`)
) ENGINE=InnoDB AUTO_INCREMENT=492349 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `bbs_msg`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `bbs_msg` (
  `MID` int NOT NULL AUTO_INCREMENT,
  `fromUID` mediumint NOT NULL DEFAULT '0',
  `toUID` mediumint NOT NULL DEFAULT '0',
  `content` longtext,
  `send_dt` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `send_ip` varchar(20) NOT NULL DEFAULT '',
  `new` tinyint unsigned NOT NULL DEFAULT '1',
  `deleted` tinyint unsigned NOT NULL DEFAULT '0',
  `s_deleted` tinyint unsigned NOT NULL DEFAULT '0',
  PRIMARY KEY (`MID`),
  KEY `fromUID` (`fromUID`),
  KEY `toUID` (`toUID`)
) ENGINE=InnoDB AUTO_INCREMENT=370615 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `email`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `email` (
  `ID` int NOT NULL AUTO_INCREMENT,
  `fromemail` varchar(30) DEFAULT NULL,
  `fromname` varchar(30) DEFAULT NULL,
  `toemail` varchar(30) DEFAULT NULL,
  `toname` varchar(30) DEFAULT NULL,
  `subject` varchar(80) DEFAULT NULL,
  `body` longtext,
  `set_dt` datetime DEFAULT NULL,
  `send_dt` datetime DEFAULT NULL,
  `complete` tinyint unsigned NOT NULL DEFAULT '0',
  `error` tinyint unsigned NOT NULL DEFAULT '0',
  `error_msg` varchar(255) DEFAULT NULL,
  PRIMARY KEY (`ID`),
  KEY `complete` (`complete`)
) ENGINE=InnoDB AUTO_INCREMENT=111218 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `ex_dir`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `ex_dir` (
  `FID` mediumint NOT NULL AUTO_INCREMENT,
  `dir` varchar(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci DEFAULT NULL,
  `name` varchar(50) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci DEFAULT NULL,
  `SID` smallint NOT NULL DEFAULT '0',
  `enable` tinyint unsigned NOT NULL DEFAULT '0',
  `dt` datetime DEFAULT NULL,
  PRIMARY KEY (`FID`),
  KEY `SID` (`SID`),
  KEY `dir` (`dir`(50))
) ENGINE=InnoDB AUTO_INCREMENT=856 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `ex_file`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `ex_file` (
  `AID` int NOT NULL DEFAULT '0',
  `FID` smallint NOT NULL DEFAULT '0',
  PRIMARY KEY (`AID`),
  KEY `FID` (`FID`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `friend_list`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `friend_list` (
  `ID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `fUID` mediumint NOT NULL DEFAULT '0',
  PRIMARY KEY (`ID`),
  KEY `fUID` (`fUID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=9587 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `section_class`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `section_class` (
  `CID` smallint NOT NULL AUTO_INCREMENT,
  `cname` varchar(20) DEFAULT NULL,
  `title` varchar(20) DEFAULT NULL,
  `enable` tinyint unsigned NOT NULL DEFAULT '1',
  `sort_order` smallint NOT NULL DEFAULT '10',
  PRIMARY KEY (`CID`),
  KEY `sort_order` (`sort_order`)
) ENGINE=InnoDB AUTO_INCREMENT=15 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `section_config`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `section_config` (
  `SID` smallint NOT NULL AUTO_INCREMENT,
  `sname` varchar(20) DEFAULT NULL,
  `CID` smallint NOT NULL DEFAULT '0',
  `title` varchar(20) DEFAULT NULL,
  `comment` varchar(255) DEFAULT NULL,
  `topic_retention` smallint NOT NULL DEFAULT '0',
  `announcement` longtext,
  `enable` tinyint unsigned NOT NULL DEFAULT '1',
  `exp_get` tinyint unsigned NOT NULL DEFAULT '0',
  `recommend` tinyint unsigned NOT NULL DEFAULT '0',
  `set_UID` mediumint DEFAULT '0',
  `set_dt` datetime DEFAULT NULL,
  `set_ip` varchar(20) DEFAULT NULL,
  `sort_order` smallint NOT NULL DEFAULT '10',
  `ex_gen_tm` datetime DEFAULT NULL,
  `ex_update` tinyint unsigned NOT NULL DEFAULT '0',
  `ex_menu_tm` datetime DEFAULT NULL,
  `ex_menu_update` tinyint NOT NULL DEFAULT '0',
  `read_user_level` smallint NOT NULL DEFAULT '0',
  `write_user_level` smallint NOT NULL DEFAULT '0',
  PRIMARY KEY (`SID`),
  KEY `CID` (`CID`),
  KEY `write_user_level` (`write_user_level`),
  KEY `sort_order` (`sort_order`),
  KEY `read_user_level` (`read_user_level`),
  KEY `sname` (`sname`)
) ENGINE=InnoDB AUTO_INCREMENT=62 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `section_favorite`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `section_favorite` (
  `ID` int NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `SID` smallint NOT NULL DEFAULT '0',
  PRIMARY KEY (`ID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=10906 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `section_master`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `section_master` (
  `MID` smallint NOT NULL AUTO_INCREMENT,
  `SID` smallint NOT NULL DEFAULT '0',
  `UID` mediumint NOT NULL DEFAULT '0',
  `begin_dt` datetime DEFAULT NULL,
  `end_dt` datetime DEFAULT NULL,
  `enable` tinyint unsigned NOT NULL DEFAULT '1',
  `major` tinyint unsigned NOT NULL DEFAULT '0',
  `memo` text,
  PRIMARY KEY (`MID`),
  KEY `SID` (`SID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=153 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `send_pass_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `send_pass_log` (
  `ID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `dt` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `ip` varchar(20) NOT NULL DEFAULT '',
  PRIMARY KEY (`ID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=2021 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `upload_file`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `upload_file` (
  `AID` mediumint NOT NULL AUTO_INCREMENT,
  `ref_AID` mediumint NOT NULL DEFAULT '0',
  `UID` mediumint NOT NULL DEFAULT '0',
  `size` mediumint NOT NULL DEFAULT '0',
  `filename` varchar(255) NOT NULL DEFAULT '',
  `check` tinyint unsigned NOT NULL DEFAULT '0',
  `deny` tinyint unsigned NOT NULL DEFAULT '0',
  `deleted` tinyint unsigned NOT NULL DEFAULT '0',
  PRIMARY KEY (`AID`),
  KEY `check` (`check`),
  KEY `UID` (`UID`),
  KEY `ref_AID` (`ref_AID`)
) ENGINE=InnoDB AUTO_INCREMENT=9675 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_err_login_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_err_login_log` (
  `ID` int NOT NULL AUTO_INCREMENT,
  `username` varchar(14) DEFAULT NULL,
  `password` varchar(12) DEFAULT NULL,
  `login_dt` datetime DEFAULT NULL,
  `login_ip` varchar(20) DEFAULT NULL,
  PRIMARY KEY (`ID`),
  KEY `login_dt` (`login_dt`,`login_ip`)
) ENGINE=InnoDB AUTO_INCREMENT=423240 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_life_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_life_log` (
  `ID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `set_UID` mediumint NOT NULL DEFAULT '0',
  `life` smallint NOT NULL DEFAULT '0',
  `dt` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `ip` varchar(20) NOT NULL DEFAULT '',
  PRIMARY KEY (`ID`),
  KEY `set_UID` (`set_UID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=1501 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_list`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_list` (
  `UID` mediumint NOT NULL AUTO_INCREMENT,
  `username` varchar(20) NOT NULL DEFAULT '',
  `password` varchar(64) NOT NULL DEFAULT '',
  `temp_password` varchar(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
  `enable` tinyint unsigned NOT NULL DEFAULT '1',
  `verified` tinyint unsigned NOT NULL DEFAULT '0',
  `p_login` tinyint unsigned NOT NULL DEFAULT '1',
  `p_post` tinyint unsigned NOT NULL DEFAULT '1',
  `p_msg` tinyint unsigned NOT NULL DEFAULT '1',
  PRIMARY KEY (`UID`),
  UNIQUE KEY `username` (`username`),
  KEY `verified` (`verified`),
  KEY `enable` (`enable`)
) ENGINE=InnoDB AUTO_INCREMENT=25223 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_login_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_login_log` (
  `ID` int NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `login_dt` datetime DEFAULT NULL,
  `login_ip` varchar(20) DEFAULT NULL,
  PRIMARY KEY (`ID`),
  KEY `UID` (`UID`),
  KEY `login_dt` (`login_dt`)
) ENGINE=InnoDB AUTO_INCREMENT=446987 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_modify_email_verify`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_modify_email_verify` (
  `MID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `email` varchar(30) DEFAULT NULL,
  `verify_code` varchar(10) NOT NULL DEFAULT '',
  `complete` tinyint unsigned NOT NULL DEFAULT '0',
  `dt` datetime DEFAULT NULL,
  `ip` varchar(20) DEFAULT NULL,
  PRIMARY KEY (`MID`),
  UNIQUE KEY `verify_code` (`verify_code`),
  KEY `UID` (`UID`),
  KEY `complete` (`complete`)
) ENGINE=InnoDB AUTO_INCREMENT=1002 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_modify_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_modify_log` (
  `MID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `modify_dt` datetime DEFAULT NULL,
  `modify_ip` varchar(20) DEFAULT NULL,
  `complete` tinyint unsigned NOT NULL DEFAULT '0',
  PRIMARY KEY (`MID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=3599 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_nickname`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_nickname` (
  `NID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `nickname` varchar(20) NOT NULL DEFAULT '',
  `begin_dt` datetime DEFAULT NULL,
  `begin_reason` char(1) DEFAULT NULL,
  `end_dt` datetime DEFAULT NULL,
  `end_reason` char(1) DEFAULT NULL,
  PRIMARY KEY (`NID`),
  KEY `UID` (`UID`),
  KEY `nickname` (`nickname`),
  KEY `begin_dt` (`begin_dt`),
  KEY `end_dt` (`end_dt`)
) ENGINE=InnoDB AUTO_INCREMENT=23696 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_online`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_online` (
  `SID` varchar(32) NOT NULL DEFAULT '',
  `UID` mediumint NOT NULL DEFAULT '0',
  `ip` varchar(20) NOT NULL DEFAULT '',
  `current_action` varchar(20) NOT NULL DEFAULT '',
  `login_tm` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  `last_tm` datetime DEFAULT NULL,
  PRIMARY KEY (`SID`) USING BTREE,
  KEY `login_tm` (`login_tm`),
  KEY `UID` (`UID`),
  KEY `last_tm` (`last_tm`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_pubinfo`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_pubinfo` (
  `UID` mediumint NOT NULL DEFAULT '0',
  `nickname` varchar(20) DEFAULT NULL,
  `email` varchar(30) DEFAULT NULL,
  `gender` char(1) NOT NULL DEFAULT 'M',
  `qq` varchar(10) DEFAULT NULL,
  `introduction` mediumtext,
  `photo` smallint DEFAULT '0',
  `photo_enable` tinyint(1) NOT NULL DEFAULT '0',
  `photo_ext` varchar(5) NOT NULL DEFAULT '',
  `life` smallint NOT NULL DEFAULT '15',
  `exp` mediumint DEFAULT '0',
  `visit_count` mediumint NOT NULL DEFAULT '0',
  `gender_pub` tinyint unsigned NOT NULL DEFAULT '0',
  `last_login_dt` datetime DEFAULT NULL,
  `last_logout_dt` datetime DEFAULT NULL,
  `sign_1` mediumtext,
  `sign_2` mediumtext,
  `sign_3` mediumtext,
  `upload_limit` int NOT NULL DEFAULT '1048576',
  `login_notify_dt` datetime DEFAULT NULL,
  `user_timezone` varchar(50) NOT NULL DEFAULT '',
  `game_money` int NOT NULL DEFAULT '0',
  PRIMARY KEY (`UID`),
  UNIQUE KEY `nickname` (`nickname`),
  KEY `life` (`life`),
  KEY `login_notify_dt` (`login_notify_dt`),
  KEY `exp` (`exp`),
  KEY `last_login_dt` (`last_login_dt`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_reginfo`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_reginfo` (
  `UID` mediumint NOT NULL DEFAULT '0',
  `name` varchar(10) DEFAULT NULL,
  `birthday` datetime DEFAULT NULL,
  `signup_dt` datetime DEFAULT NULL,
  `signup_ip` varchar(20) DEFAULT NULL,
  `memo` mediumtext,
  PRIMARY KEY (`UID`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_score`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_score` (
  `UID` mediumint NOT NULL DEFAULT '0',
  `score` mediumint NOT NULL DEFAULT '0',
  `last_exp` mediumint NOT NULL DEFAULT '0',
  `exp_left` mediumint NOT NULL DEFAULT '0',
  PRIMARY KEY (`UID`),
  KEY `score` (`score`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `user_score_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `user_score_log` (
  `ID` mediumint NOT NULL AUTO_INCREMENT,
  `UID` mediumint NOT NULL DEFAULT '0',
  `score_change` mediumint NOT NULL DEFAULT '0',
  `reason` varchar(50) DEFAULT NULL,
  `dt` datetime DEFAULT NULL,
  PRIMARY KEY (`ID`),
  KEY `UID` (`UID`)
) ENGINE=InnoDB AUTO_INCREMENT=39568 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `view_article_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `view_article_log` (
  `AID` int NOT NULL DEFAULT '0',
  `UID` mediumint NOT NULL DEFAULT '0',
  `dt` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (`AID`,`UID`),
  UNIQUE KEY `UID` (`UID`,`AID`),
  KEY `dt` (`dt`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
DROP TABLE IF EXISTS `visit_log`;
/*!40101 SET @saved_cs_client     = @@character_set_client */;
/*!50503 SET character_set_client = utf8mb4 */;
CREATE TABLE `visit_log` (
  `VID` int NOT NULL AUTO_INCREMENT,
  `dt` datetime DEFAULT NULL,
  `ip` varchar(20) DEFAULT NULL,
  PRIMARY KEY (`VID`)
) ENGINE=InnoDB AUTO_INCREMENT=16636480 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;
/*!40101 SET character_set_client = @saved_cs_client */;
/*!40103 SET TIME_ZONE=@OLD_TIME_ZONE */;

/*!40101 SET SQL_MODE=@OLD_SQL_MODE */;
/*!40014 SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS */;
/*!40014 SET UNIQUE_CHECKS=@OLD_UNIQUE_CHECKS */;
/*!40101 SET CHARACTER_SET_CLIENT=@OLD_CHARACTER_SET_CLIENT */;
/*!40101 SET CHARACTER_SET_RESULTS=@OLD_CHARACTER_SET_RESULTS */;
/*!40101 SET COLLATION_CONNECTION=@OLD_COLLATION_CONNECTION */;
/*!40111 SET SQL_NOTES=@OLD_SQL_NOTES */;

